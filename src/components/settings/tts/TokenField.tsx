import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";
import type { tokenEntry, TokenEntryState } from "./token";
export function TokenField({
  state,
  actions,
}: {
  state: TokenEntryState;
  actions: ReturnType<typeof tokenEntry>;
}) {
  const { t } = useTranslation();
  return (
    <form
      data-tts-token-field
      className="w-full text-xs text-text/50 space-y-2"
      onSubmit={(event) => {
        event.preventDefault();
        void actions.save();
      }}
    >
      <label className="flex items-center gap-2">
        {t("tts.token")}
        <Input
          type={state.visible ? "text" : "password"}
          autoComplete="off"
          value={state.draft}
          disabled={state.pending}
          onChange={(event) => actions.edit(event.target.value)}
        />
      </label>
      <p>{t("tts.tokenHelp")}</p>
      {state.error && <p role="alert">{t(state.error)}</p>}
      <div className="flex gap-2">
        <Button
          variant="primary-soft"
          size="sm"
          type="submit"
          disabled={state.pending}
        >
          {t("tts.saveToken")}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          type="button"
          disabled={state.pending}
          onClick={() => void actions.clear()}
        >
          {t("tts.clearToken")}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          type="button"
          disabled={state.pending}
          onClick={actions.show}
        >
          {t(state.visible ? "tts.hideToken" : "tts.showToken")}
        </Button>
      </div>
    </form>
  );
}

export function tokenCardProps(token: {
  entry: ReturnType<typeof tokenEntry>;
  state: TokenEntryState;
  present: boolean;
}) {
  return {
    tokenSet: token.present,
    setToken: token.entry.toggle,
    tokenEntry: token.state.expanded && (
      <TokenField state={token.state} actions={token.entry} />
    ),
  };
}
