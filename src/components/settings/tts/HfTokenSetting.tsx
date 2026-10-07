import { useTranslation } from "react-i18next";
import { TokenField } from "./TokenField";
import { useToken } from "./useToken";
export function HfTokenSetting() {
  const { t } = useTranslation();
  const token = useToken();
  return (
    <div className="p-4 space-y-2">
      <TokenField state={token.state} actions={token.entry} />
      <p className="text-xs text-text/50">
        {t(token.present ? "tts.tokenSet" : "tts.needsToken")}
      </p>
    </div>
  );
}
