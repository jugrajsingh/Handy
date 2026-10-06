use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use thiserror::Error;

pub const MAX_HEADER_BYTES: usize = 1024;
pub const MAX_SAMPLES: usize = 5_760_000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Load {
        model_dir: PathBuf,
        voice: String,
        threads: usize,
        #[serde(default)]
        device: crate::device::Device,
        #[serde(default)]
        cache_dir: PathBuf,
    },
    Synthesize {
        id: u64,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        speed: Option<f32>,
    },
    Cancel {
        id: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ready,
    Ok,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Header {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<crate::device::Device>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<std::collections::BTreeMap<String, crate::device::Device>>,
    pub id: u64,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub id: u64,
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("worker protocol I/O failed")]
    Io(#[from] std::io::Error),
    #[error("worker protocol JSON failed")]
    Json(#[from] serde_json::Error),
    #[error("worker protocol rejected {0}")]
    Invalid(&'static str),
    #[error("worker returned {0}")]
    Worker(String),
}

pub fn write_request(writer: &mut impl Write, request: &Request) -> Result<(), ProtocolError> {
    serde_json::to_writer(&mut *writer, request)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn read_header(reader: &mut impl BufRead) -> Result<Header, ProtocolError> {
    let mut line = Vec::new();
    reader
        .take((MAX_HEADER_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if line.len() > MAX_HEADER_BYTES || line.last() != Some(&b'\n') {
        return Err(ProtocolError::Invalid("header length"));
    }
    Ok(serde_json::from_slice(&line)?)
}

pub fn read_pcm(reader: &mut impl BufRead, expected_id: u64) -> Result<Pcm, ProtocolError> {
    let header = read_header(reader)?;
    read_pcm_after_header(reader, header, expected_id)
}
pub fn read_pcm_after_header(
    reader: &mut impl BufRead,
    header: Header,
    expected_id: u64,
) -> Result<Pcm, ProtocolError> {
    if header.id != expected_id {
        return Err(ProtocolError::Invalid("request id"));
    }
    if header.status != Status::Ok {
        return Err(ProtocolError::Worker(
            header.message.unwrap_or_else(|| "synthesis failed".into()),
        ));
    }
    let count = header
        .samples
        .ok_or(ProtocolError::Invalid("missing sample count"))?;
    let rate = header
        .sample_rate
        .ok_or(ProtocolError::Invalid("missing sample rate"))?;
    if count == 0 || count > MAX_SAMPLES || !(8000..=48000).contains(&rate) {
        return Err(ProtocolError::Invalid("PCM bounds"));
    }
    let mut bytes = vec![
        0u8;
        count
            .checked_mul(4)
            .ok_or(ProtocolError::Invalid("sample overflow"))?
    ];
    reader.read_exact(&mut bytes)?;
    let samples: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes([v[0], v[1], v[2], v[3]]))
        .collect();
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err(ProtocolError::Invalid("non-finite PCM"));
    }
    Ok(Pcm {
        id: header.id,
        sample_rate: rate,
        samples,
    })
}

pub fn write_reply(
    writer: &mut impl Write,
    header: &Header,
    samples: &[f32],
) -> Result<(), ProtocolError> {
    serde_json::to_writer(&mut *writer, header)?;
    writer.write_all(b"\n")?;
    for sample in samples {
        writer.write_all(&sample.to_le_bytes())?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn header_status_rate_count_and_finite_guards_return_specific_errors_before_pcm_allocation() {
        let valid = Header {
            device: None,
            placement: None,
            id: 7,
            status: Status::Ok,
            sample_rate: Some(24000),
            samples: Some(1),
            message: None,
        };
        for (header, expected) in [
            (
                Header {
                    status: Status::Error,
                    ..valid.clone()
                },
                "worker",
            ),
            (
                Header {
                    samples: Some(0),
                    ..valid.clone()
                },
                "PCM bounds",
            ),
            (
                Header {
                    samples: Some(MAX_SAMPLES + 1),
                    ..valid.clone()
                },
                "PCM bounds",
            ),
            (
                Header {
                    sample_rate: Some(0),
                    ..valid.clone()
                },
                "PCM bounds",
            ),
        ] {
            let result = read_pcm_after_header(&mut Cursor::new([0u8; 4]), header, 7);
            match result {
                Err(ProtocolError::Worker(_)) => assert_eq!(expected, "worker"),
                Err(ProtocolError::Invalid(message)) => assert_eq!(message, expected),
                other => panic!("wrong error: {other:?}"),
            }
        }
        let mut bytes = serde_json::to_vec(&valid).unwrap();
        bytes.extend(vec![b' '; MAX_HEADER_BYTES]);
        bytes.push(b'\n');
        assert!(matches!(
            read_header(&mut Cursor::new(bytes)),
            Err(ProtocolError::Invalid("header length"))
        ));
    }
    #[test]
    fn decodes_binary_without_consuming_next_header() {
        let mut bytes = Vec::new();
        for id in [7, 8] {
            write_reply(
                &mut bytes,
                &Header {
                    device: None,
                    placement: None,
                    id,
                    status: Status::Ok,
                    sample_rate: Some(24000),
                    samples: Some(2),
                    message: None,
                },
                &[0.25, -0.5],
            )
            .unwrap();
        }
        let mut reader = Cursor::new(bytes);
        assert_eq!(read_pcm(&mut reader, 7).unwrap().samples, [0.25, -0.5]);
        assert_eq!(read_pcm(&mut reader, 8).unwrap().id, 8);
    }
    #[test]
    fn rejects_truncation_nan_wrong_id_and_unbounded_header() {
        let cases: Vec<(Header, Vec<f32>)> = vec![
            (
                Header {
                    device: None,
                    placement: None,
                    id: 7,
                    status: Status::Ok,
                    sample_rate: Some(24000),
                    samples: Some(2),
                    message: None,
                },
                vec![0.0],
            ),
            (
                Header {
                    device: None,
                    placement: None,
                    id: 7,
                    status: Status::Ok,
                    sample_rate: Some(24000),
                    samples: Some(1),
                    message: None,
                },
                vec![f32::NAN],
            ),
            (
                Header {
                    device: None,
                    placement: None,
                    id: 8,
                    status: Status::Ok,
                    sample_rate: Some(24000),
                    samples: Some(1),
                    message: None,
                },
                vec![0.0],
            ),
            (
                Header {
                    device: None,
                    placement: None,
                    id: 7,
                    status: Status::Ok,
                    sample_rate: Some(24000),
                    samples: Some(MAX_SAMPLES + 1),
                    message: None,
                },
                vec![],
            ),
            (
                Header {
                    device: None,
                    placement: None,
                    id: 7,
                    status: Status::Ok,
                    sample_rate: Some(0),
                    samples: Some(1),
                    message: None,
                },
                vec![0.0],
            ),
        ];
        for (header, pcm) in cases {
            let mut bytes = Vec::new();
            write_reply(&mut bytes, &header, &pcm).unwrap();
            assert!(read_pcm(&mut Cursor::new(bytes), 7).is_err());
        }
        assert!(read_header(&mut Cursor::new(vec![b'x'; MAX_HEADER_BYTES + 1])).is_err());
    }
    #[test]
    fn serializes_controls_and_omits_pocket_native_speed() {
        for request in [
            Request::Load {
                device: crate::device::Device::Cpu,
                cache_dir: PathBuf::new(),
                model_dir: "/models/pocket-en".into(),
                voice: "alba".into(),
                threads: 4,
            },
            Request::Synthesize {
                id: 1,
                text: "Read this.".into(),
                speed: None,
            },
            Request::Cancel { id: 1 },
            Request::Shutdown,
        ] {
            let mut bytes = Vec::new();
            write_request(&mut bytes, &request).unwrap();
            assert_eq!(serde_json::from_slice::<Request>(&bytes).unwrap(), request);
            if matches!(request, Request::Synthesize { .. }) {
                assert!(!String::from_utf8(bytes).unwrap().contains("speed"));
            }
        }
    }
}
