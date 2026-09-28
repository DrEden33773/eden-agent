import { useEffect, useState } from "react";

export type Binding = {
  instance: string;
  generation: number | null;
  revision: number;
  profile: number;
};
export type ConfigurationField = {
  path: string;
  label: string;
  description?: string | null;
  control: "text" | "boolean" | "integer" | "number" | "choice" | "list" | "json" | "secret";
  value?: unknown;
  item_kind?: string | null;
  options: unknown[];
  source?: string | null;
  writable: boolean;
  configured: boolean;
};
export type ConfigurationNode = {
  kind: "configuration_form";
  id: string;
  binding: Binding;
  fields: ConfigurationField[];
};
export type Edit =
  | { operation: "set"; path: string; value: unknown }
  | { operation: "clear" | "inherit"; path: string };
export type ConfigurationDraft = { binding: Binding; edits: Record<string, Edit>; fields: string };
export function bindingIdentity(binding: Binding): string {
  return JSON.stringify([binding.instance, binding.generation, binding.revision, binding.profile]);
}
export function fieldIdentity(fields: ConfigurationField[]): string {
  return JSON.stringify(
    fields.map((field) => [
      field.path,
      field.control,
      field.writable,
      field.options,
      field.item_kind,
    ]),
  );
}
function ValueInput({
  value,
  control,
  label,
  change,
}: {
  value: unknown;
  control: string;
  label: string;
  change: (value: unknown) => void;
}) {
  const [raw, setRaw] = useState<string | null>(null);
  const [error, setError] = useState("");
  if (control === "boolean")
    return (
      <label>
        <input
          type="checkbox"
          checked={value === true}
          onChange={(event) => change(event.target.checked)}
        />
        {label}
      </label>
    );
  const text = control === "text" ? String(value ?? "") : JSON.stringify(value ?? null);
  return (
    <label>
      {label}
      <input
        aria-invalid={Boolean(error)}
        value={raw ?? text}
        onChange={(event) => {
          const next = event.target.value;
          setRaw(next);
          try {
            const parsed: unknown = control === "text" ? next : JSON.parse(next);
            if (
              (control === "integer" || control === "number") &&
              (typeof parsed !== "number" ||
                !Number.isFinite(parsed) ||
                (control === "integer" && !Number.isInteger(parsed)))
            )
              throw new Error(`Enter a valid ${control}`);
            if (
              control === "object" &&
              (parsed === null || typeof parsed !== "object" || Array.isArray(parsed))
            )
              throw new Error("Enter a JSON object");
            if (control === "array" && !Array.isArray(parsed))
              throw new Error("Enter a JSON array");
            change(parsed);
            setError("");
          } catch {
            setError(`Enter valid ${control === "json" ? "JSON" : control}`);
          }
        }}
      />
      {error && <span role="alert">{error}</span>}
    </label>
  );
}
let rowId = 0;
function ListInput({
  value,
  itemKind,
  change,
}: {
  value: unknown;
  itemKind?: string | null;
  change: (value: unknown) => void;
}) {
  const items = Array.isArray(value) ? value : [];
  const [ids, setIds] = useState(() => items.map(() => ++rowId));
  const update = (next: unknown[], nextIds = ids) => {
    setIds(nextIds);
    change(next);
  };
  return (
    <div>
      {items.map((item, index) => (
        <div key={ids[index]} className="controls">
          <ValueInput
            value={item}
            control={
              itemKind
                ? itemKind === "string"
                  ? "text"
                  : itemKind
                : typeof item === "string"
                  ? "text"
                  : typeof item === "boolean"
                    ? "boolean"
                    : typeof item === "number"
                      ? "number"
                      : "json"
            }
            label={`Item ${index + 1}${typeof item === "object" || itemKind === "object" || itemKind === "array" ? " (JSON fallback)" : ""}`}
            change={(next) => update(items.map((old, at) => (at === index ? next : old)))}
          />
          <button
            type="button"
            onClick={() =>
              update(
                items.filter((_, at) => at !== index),
                ids.filter((_, at) => at !== index),
              )
            }
          >
            Delete item {index + 1}
          </button>
          <button
            type="button"
            disabled={index === 0}
            onClick={() => {
              const next = [...items];
              const nextIds = [...ids];
              [next[index - 1], next[index]] = [next[index], next[index - 1]];
              [nextIds[index - 1], nextIds[index]] = [nextIds[index], nextIds[index - 1]];
              update(next, nextIds);
            }}
          >
            Move item {index + 1} up
          </button>
          <button
            type="button"
            disabled={index === items.length - 1}
            onClick={() => {
              const next = [...items];
              const nextIds = [...ids];
              [next[index + 1], next[index]] = [next[index], next[index + 1]];
              [nextIds[index + 1], nextIds[index]] = [nextIds[index], nextIds[index + 1]];
              update(next, nextIds);
            }}
          >
            Move item {index + 1} down
          </button>
        </div>
      ))}
      <button
        type="button"
        onClick={() =>
          update(
            [
              ...items,
              itemKind === "integer" || itemKind === "number"
                ? 0
                : itemKind === "boolean"
                  ? false
                  : itemKind === "object"
                    ? {}
                    : itemKind === "array"
                      ? []
                      : itemKind === "string"
                        ? ""
                        : typeof items[0] === "number"
                          ? 0
                          : typeof items[0] === "boolean"
                            ? false
                            : typeof items[0] === "string"
                              ? ""
                              : null,
            ],
            [...ids, ++rowId],
          )
        }
      >
        Add item
      </button>
    </div>
  );
}
export function ConfigurationForm({
  node,
  draft,
  save,
  disabled,
  act,
}: {
  node: ConfigurationNode;
  draft?: ConfigurationDraft;
  save: (draft?: ConfigurationDraft) => void;
  disabled: boolean;
  act: (action: string, values: unknown, inputs?: Edit[]) => Promise<unknown>;
}) {
  const [privateEdits, setPrivateEdits] = useState<Record<string, Edit>>({});
  const bindingKey = bindingIdentity(node.binding);
  const fieldsKey = fieldIdentity(node.fields);
  // biome-ignore lint/correctness/useExhaustiveDependencies: A binding or schema change invalidates all private draft material.
  useEffect(() => {
    setPrivateEdits({});
  }, [bindingKey, fieldsKey]);
  const [busy, setBusy] = useState(false);
  const [reset, setReset] = useState(0);
  const discard = () => {
    setPrivateEdits({});
    save(undefined);
    setReset((value) => value + 1);
  };
  const edits = draft?.edits ?? {};
  const conflict = Boolean(
    draft &&
      (bindingIdentity(draft.binding) !== bindingIdentity(node.binding) ||
        draft.fields !== fieldIdentity(node.fields)),
  );
  const edit = (value: Edit) =>
    save({
      binding: draft?.binding ?? node.binding,
      fields: draft?.fields ?? fieldIdentity(node.fields),
      edits: { ...edits, [value.path]: value },
    });
  return (
    <form className="configuration" onSubmit={(event) => event.preventDefault()}>
      <p>
        Instance {node.binding.instance} · configuration revision {node.binding.revision} ·
        generation {node.binding.generation ?? "not running"}
      </p>
      {conflict && (
        <div role="alert">
          Configuration changed. Your draft is retained and cannot be submitted to the new binding.
          <pre>{JSON.stringify(edits, null, 2)}</pre>
        </div>
      )}
      {node.fields.map((field) => {
        const pending = edits[field.path];
        const value = pending?.operation === "set" ? pending.value : field.value;
        return (
          <fieldset
            key={`${reset}/${bindingIdentity(node.binding)}/${field.path}`}
            disabled={disabled || busy || conflict || !field.writable}
          >
            <legend>
              {field.label} ({field.path})
            </legend>
            <p>
              {field.description} · Source: {field.source ?? "unspecified"}
              {!field.writable && " · Read only"}
            </p>
            {field.control === "secret" ? (
              <>
                <p>{field.configured ? "Configured" : "Not configured"}</p>
                <label>
                  {field.configured ? "Replace secret" : "Set secret"}
                  <input
                    type="password"
                    autoComplete="new-password"
                    value={
                      privateEdits[field.path]?.operation === "set"
                        ? String((privateEdits[field.path] as { value: unknown }).value)
                        : ""
                    }
                    onChange={(event) =>
                      setPrivateEdits((previous) => ({
                        ...previous,
                        [field.path]: {
                          operation: "set",
                          path: field.path,
                          value: event.target.value,
                        },
                      }))
                    }
                  />
                </label>
                <button
                  type="button"
                  onClick={() =>
                    setPrivateEdits((previous) => ({
                      ...previous,
                      [field.path]: { operation: "clear", path: field.path },
                    }))
                  }
                >
                  Clear secret
                </button>
                {privateEdits[field.path] && (
                  <p>
                    Pending:{" "}
                    {privateEdits[field.path].operation === "set" ? "replacement" : "clear"}
                  </p>
                )}
              </>
            ) : (
              <>
                {field.control === "choice" ? (
                  <label>
                    {field.label}
                    <select
                      value={JSON.stringify(value) ?? ""}
                      onChange={(event) =>
                        edit({
                          operation: "set",
                          path: field.path,
                          value: JSON.parse(event.target.value),
                        })
                      }
                    >
                      <option value="" disabled>
                        Select a value
                      </option>
                      {field.options.map((option) => (
                        <option key={JSON.stringify(option)} value={JSON.stringify(option)}>
                          {JSON.stringify(option)}
                        </option>
                      ))}
                    </select>
                  </label>
                ) : field.control === "list" ? (
                  <ListInput
                    value={value}
                    itemKind={field.item_kind}
                    change={(next) => edit({ operation: "set", path: field.path, value: next })}
                  />
                ) : (
                  <ValueInput
                    value={value}
                    control={field.control}
                    label={
                      field.control === "json" ? "JSON fallback (non-secret subtree)" : field.label
                    }
                    change={(next) => edit({ operation: "set", path: field.path, value: next })}
                  />
                )}
                <div className="controls">
                  <button
                    type="button"
                    onClick={() => edit({ operation: "clear", path: field.path })}
                  >
                    Clear
                  </button>
                  <button
                    type="button"
                    onClick={() => edit({ operation: "inherit", path: field.path })}
                  >
                    Restore inheritance
                  </button>
                </div>
                {pending && (
                  <p>
                    Pending:{" "}
                    {pending.operation === "set"
                      ? JSON.stringify(pending.value)
                      : pending.operation}
                  </p>
                )}
              </>
            )}
          </fieldset>
        );
      })}
      <div className="controls">
        {["validate", "preview", "apply", "cancel_apply", "refresh"].map((action) => (
          <button
            key={action}
            type="button"
            disabled={disabled || busy || (conflict && action !== "refresh")}
            onClick={async (event) => {
              if (
                ["validate", "preview", "apply", "cancel_apply"].includes(action) &&
                event.currentTarget.form?.querySelector('[aria-invalid="true"]')
              )
                return;
              const inputs = action === "refresh" ? [] : Object.values(privateEdits);
              setPrivateEdits({});
              setBusy(true);
              try {
                const result = await act(
                  `${node.id}:${action}`,
                  {
                    binding: action === "refresh" ? node.binding : (draft?.binding ?? node.binding),
                    edits: action === "refresh" ? [] : Object.values(edits),
                  },
                  inputs.length ? inputs : undefined,
                );
                if (
                  (action === "apply" || action === "cancel_apply") &&
                  (result as { status?: string } | undefined)?.status === "applied"
                )
                  discard();
              } finally {
                setBusy(false);
              }
            }}
          >
            {action.replaceAll("_", " ")}
          </button>
        ))}
        <button type="button" onClick={discard}>
          Discard draft and use current values
        </button>
      </div>
      {busy && <p role="status">Configuration request pending…</p>}
    </form>
  );
}
