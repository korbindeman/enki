import { createSignal, onMount, Show, For } from "solid-js";
import { invoke } from "@tauri-apps/api/core";

interface ConfigPayload {
  commit_suffix: string;
  max_workers: number;
  max_heavy: number;
  max_standard: number;
  max_light: number;
  sonnet_only: boolean;
  agent_command: string;
}

const DEFAULTS: ConfigPayload = {
  commit_suffix: "created by enki",
  max_workers: 10,
  max_heavy: 5,
  max_standard: 5,
  max_light: 10,
  sonnet_only: false,
  agent_command: "claude-agent-acp",
};

type SettingConfig =
  | { type: "number"; key: keyof ConfigPayload; label: string; min: number; max: number }
  | { type: "text"; key: keyof ConfigPayload; label: string }
  | { type: "boolean"; key: keyof ConfigPayload; label: string }
  | { type: "separator" }
  | { type: "heading"; label: string };

const settingsConfig: SettingConfig[] = [
  { type: "heading", label: "Workers" },
  { type: "number", key: "max_workers", label: "Max Workers", min: 1, max: 50 },
  { type: "number", key: "max_heavy", label: "Max Heavy", min: 0, max: 50 },
  { type: "number", key: "max_standard", label: "Max Standard", min: 0, max: 50 },
  { type: "number", key: "max_light", label: "Max Light", min: 0, max: 50 },
  { type: "boolean", key: "sonnet_only", label: "Sonnet Only" },
  { type: "separator" },
  { type: "heading", label: "Git" },
  { type: "text", key: "commit_suffix", label: "Commit Suffix" },
  { type: "separator" },
  { type: "heading", label: "Agent" },
  { type: "text", key: "agent_command", label: "Command" },
];

export default function Settings(props: { open: boolean; onClose: () => void }) {
  const [config, setConfig] = createSignal<ConfigPayload | null>(null);
  const [loading, setLoading] = createSignal(true);

  onMount(async () => {
    const loaded = await invoke<ConfigPayload>("load_config");
    setConfig(loaded);
    setLoading(false);
  });

  const updateField = async <K extends keyof ConfigPayload>(key: K, value: ConfigPayload[K]) => {
    const current = config();
    if (!current) return;
    const updated = { ...current, [key]: value };
    setConfig(updated);
    await invoke("save_config", { config: updated });
  };

  const resetField = <K extends keyof ConfigPayload>(key: K) => {
    updateField(key, DEFAULTS[key]);
  };

  function handleBackdrop(e: MouseEvent) {
    if (e.target === e.currentTarget) props.onClose();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") props.onClose();
  }

  return (
    <Show when={props.open}>
      <div
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
        onClick={handleBackdrop}
        onKeyDown={handleKeydown}
      >
        <div class="w-[420px] bg-surface border border-border-subtle rounded-xl px-5 py-5 pb-7 shadow-xl">
          <Show when={!loading() && config()} fallback={
            <div class="text-text-muted text-sm">Loading settings...</div>
          }>
            <div class="space-y-3">
              <h2 class="text-base font-semibold text-text pb-1">Settings</h2>
              <p class="text-xs text-text-muted">
                Global config (~/.config/enki.toml). Restart to apply changes.
              </p>
              <For each={settingsConfig}>
                {(setting) => {
                  const current = config()!;

                  if (setting.type === "separator") {
                    return <hr class="border-border" />;
                  }

                  if (setting.type === "heading") {
                    return (
                      <div class="text-xs font-medium text-text-muted uppercase tracking-wide pt-1">
                        {setting.label}
                      </div>
                    );
                  }

                  if (setting.type === "number") {
                    const value = current[setting.key] as number;
                    const defaultValue = DEFAULTS[setting.key] as number;
                    const hasChanged = value !== defaultValue;
                    return (
                      <div class="flex items-center justify-between">
                        <label class="text-sm text-text">{setting.label}</label>
                        <div class="flex items-center gap-2">
                          <Show when={hasChanged}>
                            <button
                              class="text-xs text-text-muted hover:text-text hover:underline"
                              onClick={() => resetField(setting.key)}
                            >
                              Reset
                            </button>
                          </Show>
                          <div class="flex items-center">
                            <button
                              class="w-7 h-7 flex items-center justify-center rounded-l border border-border bg-button-bg text-text hover:bg-button-hover text-sm"
                              onClick={() => {
                                const next = Math.max(setting.min, value - 1);
                                updateField(setting.key, next);
                              }}
                            >
                              -
                            </button>
                            <input
                              type="number"
                              class="w-12 h-7 text-center text-sm bg-input-bg border-y border-border text-text appearance-none [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none"
                              value={value}
                              min={setting.min}
                              max={setting.max}
                              onInput={(e) => {
                                const v = parseInt(e.currentTarget.value);
                                if (!isNaN(v)) {
                                  updateField(setting.key, Math.min(setting.max, Math.max(setting.min, v)));
                                }
                              }}
                            />
                            <button
                              class="w-7 h-7 flex items-center justify-center rounded-r border border-border bg-button-bg text-text hover:bg-button-hover text-sm"
                              onClick={() => {
                                const next = Math.min(setting.max, value + 1);
                                updateField(setting.key, next);
                              }}
                            >
                              +
                            </button>
                          </div>
                        </div>
                      </div>
                    );
                  }

                  if (setting.type === "text") {
                    const value = current[setting.key] as string;
                    const defaultValue = DEFAULTS[setting.key] as string;
                    const hasChanged = value !== defaultValue;
                    return (
                      <div class="flex items-center justify-between">
                        <label class="text-sm text-text">{setting.label}</label>
                        <div class="flex items-center gap-2">
                          <Show when={hasChanged}>
                            <button
                              class="text-xs text-text-muted hover:text-text hover:underline"
                              onClick={() => resetField(setting.key)}
                            >
                              Reset
                            </button>
                          </Show>
                          <input
                            type="text"
                            class="w-48 h-7 px-2 text-sm bg-input-bg border border-border rounded text-text focus:outline-none focus:border-text-muted"
                            value={value}
                            onInput={(e) => updateField(setting.key, e.currentTarget.value)}
                          />
                        </div>
                      </div>
                    );
                  }

                  if (setting.type === "boolean") {
                    const value = current[setting.key] as boolean;
                    const defaultValue = DEFAULTS[setting.key] as boolean;
                    const hasChanged = value !== defaultValue;
                    return (
                      <div class="flex items-center justify-between">
                        <label class="text-sm text-text">{setting.label}</label>
                        <div class="flex items-center gap-2">
                          <Show when={hasChanged}>
                            <button
                              class="text-xs text-text-muted hover:text-text hover:underline"
                              onClick={() => resetField(setting.key)}
                            >
                              Reset
                            </button>
                          </Show>
                          <button
                            class={`w-5 h-5 rounded border flex items-center justify-center text-xs ${
                              value
                                ? "bg-button-hover border-border text-text"
                                : "bg-input-bg border-border text-transparent"
                            }`}
                            onClick={() => updateField(setting.key, !value)}
                          >
                            ✓
                          </button>
                        </div>
                      </div>
                    );
                  }

                  return null;
                }}
              </For>
            </div>
          </Show>
        </div>
      </div>
    </Show>
  );
}
