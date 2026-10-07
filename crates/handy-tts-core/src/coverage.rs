/// Word coverage measured against an order-preserving transcript alignment.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Coverage {
    pub expected: usize,
    pub matched: usize,
    pub recall: f64,
    pub longest_missing_run: usize,
    pub missing: Vec<(usize, String)>,
}

/// Lowercases and folds Latin letters to ASCII, retaining digits and possessive apostrophes.
pub fn normalize_words(text: &str) -> Vec<String> {
    let mut folded = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        let replacement = match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
            'ď' | 'đ' | 'ð' => "d",
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
            'ĥ' | 'ħ' => "h",
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
            'ĵ' => "j",
            'ķ' => "k",
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
            'ñ' | 'ń' | 'ņ' | 'ň' | 'ŋ' => "n",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
            'ŕ' | 'ŗ' | 'ř' => "r",
            'ś' | 'ŝ' | 'ş' | 'š' | 'ſ' => "s",
            'ţ' | 'ť' | 'ŧ' => "t",
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
            'ŵ' => "w",
            'ý' | 'ÿ' | 'ŷ' => "y",
            'ź' | 'ż' | 'ž' => "z",
            'æ' => "ae",
            'œ' => "oe",
            'ß' => "ss",
            'þ' => "th",
            '\u{0300}'..='\u{036f}' => "",
            '\'' | '’' | '‘' => "'",
            c if c.is_ascii_alphanumeric() => {
                folded.push(c);
                continue;
            }
            _ => " ",
        };
        folded.push_str(replacement);
    }
    folded
        .split_whitespace()
        .flat_map(|word| {
            let chars: Vec<char> = word.trim_matches('\'').chars().collect();
            let separated: String = chars
                .iter()
                .enumerate()
                .map(|(i, &c)| {
                    if c == '\''
                        && !(i > 0
                            && chars[i - 1].is_ascii_alphanumeric()
                            && chars.get(i + 1) == Some(&'s')
                            && i + 2 == chars.len())
                    {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            separated
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Aligns words by longest common subsequence and reports unmatched expected indices.
pub fn align(expected: &[String], heard: &[String]) -> Coverage {
    let columns = heard.len() + 1;
    let mut lengths = vec![0; (expected.len() + 1) * columns];
    for i in (0..expected.len()).rev() {
        for j in (0..heard.len()).rev() {
            lengths[i * columns + j] = if expected[i] == heard[j] {
                1 + lengths[(i + 1) * columns + j + 1]
            } else {
                lengths[(i + 1) * columns + j].max(lengths[i * columns + j + 1])
            };
        }
    }
    let mut coverage = Coverage {
        expected: expected.len(),
        matched: lengths[0],
        recall: if expected.is_empty() {
            1.0
        } else {
            lengths[0] as f64 / expected.len() as f64
        },
        longest_missing_run: 0,
        missing: Vec::new(),
    };
    let (mut i, mut j, mut run) = (0, 0, 0);
    while i < expected.len() {
        if j < heard.len() && expected[i] == heard[j] {
            run = 0;
            i += 1;
            j += 1;
        } else if j < heard.len() && lengths[i * columns + j + 1] > lengths[(i + 1) * columns + j] {
            j += 1;
        } else {
            coverage.missing.push((i, expected[i].clone()));
            run += 1;
            coverage.longest_missing_run = coverage.longest_missing_run.max(run);
            i += 1;
        }
    }
    coverage
}

/// Checks that each contiguous anchor phrase occurs after the preceding phrase.
pub fn anchors_in_order(heard: &[String], anchors: &[&[&str]]) -> bool {
    let mut position = 0;
    for anchor in anchors {
        if anchor.is_empty() {
            continue;
        }
        let Some(offset) = heard[position..].windows(anchor.len()).position(|words| {
            words
                .iter()
                .zip(*anchor)
                .all(|(word, expected)| word == expected)
        }) else {
            return false;
        };
        position += offset + anchor.len();
    }
    true
}

/// Counts consecutive losses among words heard in the same input's baseline.
pub fn longest_missing_loss_run(at_speed: &Coverage, baseline: &Coverage) -> usize {
    let baseline_missing: std::collections::BTreeSet<_> =
        baseline.missing.iter().map(|(index, _)| *index).collect();
    let current_missing: std::collections::BTreeSet<_> =
        at_speed.missing.iter().map(|(index, _)| *index).collect();
    let (mut run, mut longest) = (0, 0);
    for index in 0..at_speed.expected {
        if baseline_missing.contains(&index) {
            continue;
        }
        if current_missing.contains(&index) {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// Requires each anchor's baseline-heard words in an ordered expected-text alignment.
pub fn relative_anchors_in_order(
    expected: &[String],
    heard: &[String],
    baseline: &Coverage,
    anchors: &[&[&str]],
) -> bool {
    if baseline.expected != expected.len() {
        return false;
    }
    let at_speed = align(expected, heard);
    let baseline_missing: std::collections::BTreeSet<_> =
        baseline.missing.iter().map(|(index, _)| *index).collect();
    let current_missing: std::collections::BTreeSet<_> =
        at_speed.missing.iter().map(|(index, _)| *index).collect();
    let mut position = 0;
    for anchor in anchors {
        if anchor.is_empty() {
            continue;
        }
        let Some(offset) = expected[position..]
            .windows(anchor.len())
            .position(|words| {
                words
                    .iter()
                    .zip(*anchor)
                    .all(|(word, anchor)| word == anchor)
            })
        else {
            return false;
        };
        let start = position + offset;
        position = start + anchor.len();
        let retained: Vec<_> = (start..position)
            .filter(|index| !baseline_missing.contains(index))
            .collect();
        if retained.is_empty() || retained.iter().any(|index| current_missing.contains(index)) {
            return false;
        }
    }
    true
}

/// Trusts a plain-prose calibration only at or above ninety-five percent recall.
pub fn calibration_passes(coverage: &Coverage) -> bool {
    coverage.recall >= 0.95
}

/// Accepts isolated new losses and at most a two-percentage-point recall drop.
pub fn passes(at_speed: &Coverage, baseline: &Coverage) -> bool {
    at_speed.expected == baseline.expected
        && longest_missing_loss_run(at_speed, baseline) < 2
        && at_speed.recall >= baseline.recall - 0.02
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const OWNER: &str = include_str!("../tests/fixtures/speech/owner-2x.txt");

    #[test]
    fn normalizes_punctuation_case_and_possessives() {
        assert_eq!(
            normalize_words("UI1's findings: 'Pocket's'"),
            ["ui1's", "findings", "pocket's"]
        );
        assert_eq!(
            normalize_words("Café, naïve! POCKET’S 2.0 café\u{301}"),
            ["cafe", "naive", "pocket's", "2", "0", "cafe"]
        );
    }

    #[test]
    fn owner_fixture_missing_chunk_one_fails_with_run_of_nine() {
        let expected = normalize_words(OWNER);
        let heard = normalize_words(
            &OWNER.replace("and the token entry and section grouping get tests", ""),
        );
        let coverage = align(&expected, &heard);
        assert_eq!(coverage.longest_missing_run, 9);
        assert_eq!(coverage.missing.len(), 9);
        assert!(!passes(&coverage, &align(&expected, &expected)));
        assert!(passes(&coverage, &coverage));
        let long_expected = normalize_words(&format!("{OWNER}{}", " padding".repeat(500)));
        let long_heard = normalize_words(&format!(
            "{}{}",
            OWNER.replace("and the token entry and section grouping get tests", ""),
            " padding".repeat(500)
        ));
        assert!(!passes(
            &align(&long_expected, &long_heard),
            &align(&long_expected, &long_expected)
        ));
    }

    #[test]
    fn single_asr_substitution_is_tolerated() {
        let expected = normalize_words(&"word ".repeat(100));
        let mut heard = expected.clone();
        heard[50] = "substitution".into();
        let coverage = align(&expected, &heard);
        assert_eq!(coverage.longest_missing_run, 1);
        assert_eq!(coverage.matched, 99);
        assert!(passes(&coverage, &align(&expected, &expected)));
        let short = normalize_words("a b c");
        assert!(!passes(
            &align(&short, &normalize_words("a x c")),
            &align(&short, &short)
        ));
    }

    #[test]
    fn anchors_must_be_in_order() {
        let anchors: &[&[&str]] = &[&["is", "set"], &["token", "entry"], &["get", "tests"]];
        assert!(anchors_in_order(
            &normalize_words("is set and token entry get tests"),
            anchors
        ));
        assert!(!anchors_in_order(
            &normalize_words("token entry is set get tests"),
            anchors
        ));
        assert!(!anchors_in_order(
            &normalize_words("is now set token entry get tests"),
            anchors
        ));
        assert!(!anchors_in_order(
            &normalize_words("is set token entry"),
            anchors
        ));
    }

    #[test]
    fn order_matters_in_alignment() {
        let expected = normalize_words("one two three four");
        let heard = normalize_words("four three two one");
        let coverage = align(&expected, &heard);
        assert_eq!(coverage.matched, 1);
        assert_eq!(coverage.recall, 0.25);
        assert_eq!(coverage.missing.len(), 3);
    }

    #[test]
    fn alignment_reports_indices_and_handles_empty_inputs() {
        let expected = normalize_words("a b c d e");
        let coverage = align(&expected, &normalize_words("extra a d extra e"));
        assert_eq!(coverage.missing, [(1, "b".into()), (2, "c".into())]);
        assert_eq!(coverage.longest_missing_run, 2);
        assert_eq!(coverage.expected, 5);
        assert_eq!(align(&expected, &[]).longest_missing_run, 5);
        assert_eq!(align(&[], &[]).recall, 1.0);
    }

    #[test]
    fn baseline_missed_tokens_never_count_as_losses() {
        let expected = normalize_words(&format!("jargon names a middle b{}", " tail".repeat(100)));
        let baseline_heard = normalize_words(&format!("a b{}", " tail".repeat(100)));
        let baseline = align(&expected, &baseline_heard);
        assert_eq!(baseline.longest_missing_run, 2);
        assert!(passes(&baseline, &baseline));
        let one_loss = align(
            &expected,
            &normalize_words(&format!("b{}", " tail".repeat(100))),
        );
        assert_eq!(longest_missing_loss_run(&one_loss, &baseline), 1);
        assert!(passes(&one_loss, &baseline));
        let two_losses = align(&expected, &normalize_words(&"tail ".repeat(100)));
        assert_eq!(longest_missing_loss_run(&two_losses, &baseline), 2);
        assert!(!passes(&two_losses, &baseline));
    }

    #[test]
    fn relative_anchors_ignore_only_baseline_misses_and_keep_order() {
        let expected =
            normalize_words("is set then come jargon settings page and other popup words");
        let baseline_heard =
            normalize_words("is set then come misheard settings page and mistaken words");
        let baseline = align(&expected, &baseline_heard);
        let anchors: &[&[&str]] = &[
            &["is", "set"],
            &[
                "then", "come", "jargon", "settings", "page", "and", "other", "popup", "words",
            ],
        ];
        assert!(relative_anchors_in_order(
            &expected,
            &baseline_heard,
            &baseline,
            anchors
        ));
        assert!(!relative_anchors_in_order(
            &expected,
            &normalize_words("is set then come settings and words"),
            &baseline,
            anchors
        ));
        assert!(!relative_anchors_in_order(
            &expected,
            &normalize_words("then come settings page and words is set"),
            &baseline,
            anchors
        ));
        assert!(!relative_anchors_in_order(
            &expected,
            &baseline_heard,
            &baseline,
            &[&["jargon"]]
        ));
    }

    #[test]
    fn calibration_requires_at_least_ninety_five_percent_recall() {
        let expected = normalize_words(&"word ".repeat(100));
        assert!(!calibration_passes(&align(
            &expected,
            &normalize_words(&"word ".repeat(94))
        )));
        assert!(calibration_passes(&align(
            &expected,
            &normalize_words(&"word ".repeat(95))
        )));
    }

    #[test]
    fn calibration_fixture_is_plain_prose_with_eighty_to_one_hundred_twenty_words() {
        let prose = include_str!("../tests/fixtures/speech/calibration-prose.txt");
        assert!((80..=120).contains(&normalize_words(prose).len()));
        assert!(!prose.chars().any(|c| c.is_ascii_digit()));
        assert!(prose.is_ascii());
    }

    #[test]
    fn fixture_bytes_are_the_bug2_input() {
        assert_eq!(OWNER.len(), 225);
        assert!(!OWNER.ends_with('\n'));
        assert_eq!(
            format!("{:x}", Sha256::digest(OWNER.as_bytes())),
            "fbf539f26e71be72fbb93f0570217728d1129c64e13a344bc58a56ca2746c8f6"
        );
    }
}
