import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ModelKeepAliveField } from "@/components/providers/forms/ModelKeepAliveField";
import {
  modelKeepAliveConfig,
  validModelKeepAlive,
} from "@/lib/modelKeepAlive";
import type { ModelKeepAliveConfig } from "@/types";

const RETRY = "modelKeepAlive.retryIntervalSeconds";
const INTERVAL = "modelKeepAlive.intervalSeconds";

function Editor({
  initial = modelKeepAliveConfig({ enabled: true }),
  onSave,
}: {
  initial?: ModelKeepAliveConfig;
  onSave: (config: ModelKeepAliveConfig) => void;
}) {
  const [config, setConfig] = useState(initial);
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        if (validModelKeepAlive(config)) {
          onSave(JSON.parse(JSON.stringify(config)));
        }
      }}
    >
      <ModelKeepAliveField
        model="test-model"
        value={config}
        onChange={setConfig}
      />
      <button type="submit" disabled={!validModelKeepAlive(config)}>
        Save
      </button>
    </form>
  );
}

describe("ModelKeepAliveField zero retry interval", () => {
  it("saves zero and displays it again after reopening", async () => {
    const user = userEvent.setup();
    const save = vi.fn();
    const editor = render(<Editor onSave={save} />);
    const retry = screen.getByRole("spinbutton", { name: RETRY });
    await user.clear(retry);
    await user.type(retry, "0");
    expect(retry).toHaveValue(0);
    expect(retry).toHaveAttribute("min", "0");
    expect(retry).toHaveAttribute("aria-invalid", "false");
    expect(
      screen.queryByText("modelKeepAlive.invalid"),
    ).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(save).toHaveBeenCalledWith(
      expect.objectContaining({ retryIntervalSeconds: 0, intervalSeconds: 60 }),
    );
    editor.unmount();
    render(<Editor initial={save.mock.calls[0][0]} onSave={save} />);
    expect(screen.getByRole("spinbutton", { name: RETRY })).toHaveValue(0);
  });

  it("keeps a cleared interval invalid instead of silently saving zero", async () => {
    const user = userEvent.setup();
    const save = vi.fn();
    render(
      <Editor
        initial={modelKeepAliveConfig({
          enabled: true,
          retryIntervalSeconds: 0,
        })}
        onSave={save}
      />,
    );
    const retry = screen.getByRole("spinbutton", { name: RETRY });
    await user.clear(retry);
    expect(retry).toHaveValue(null);
    expect(retry).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(save).not.toHaveBeenCalled();
  });

  it.each([-1, 0.5, 86401])(
    "rejects an invalid retry interval of %s",
    (value) => {
      render(<Editor onSave={vi.fn()} />);
      const retry = screen.getByRole("spinbutton", { name: RETRY });
      fireEvent.change(retry, { target: { value: String(value) } });
      expect(retry).toHaveAttribute("aria-invalid", "true");
      expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    },
  );

  it("still requires a positive interval after a successful request", () => {
    render(<Editor onSave={vi.fn()} />);
    const interval = screen.getByRole("spinbutton", { name: INTERVAL });
    fireEvent.change(interval, { target: { value: "0" } });
    expect(interval).toHaveAttribute("min", "1");
    expect(interval).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  });
});
