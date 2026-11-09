// A form drawn from a plugin's settings schema. The schema subset (see settingsSchema.ts) is
// exactly what the server validates, so every field here has a widget and every rule the
// server applies has an input attribute where HTML has one.

import { useId } from "react";

import { emptyValue, labelFor, parseNumber, type JsonSchema, type SecretEdit, type Values } from "./settingsSchema";

interface FieldProps {
  name: string;
  schema: JsonSchema;
  value: unknown;
  required: boolean;
  onChange: (value: unknown) => void;
}

function Help({ schema }: { schema: JsonSchema }) {
  return schema.description ? <small className="muted plugin-help">{schema.description}</small> : null;
}

function Field({ name, schema, value, required, onChange }: FieldProps) {
  const id = useId();
  const label = labelFor(name, schema);
  switch (schema.type) {
    case "boolean":
      return (
        <label className="plugin-check">
          <input type="checkbox" checked={value === true} onChange={(e) => onChange(e.target.checked)} />
          <span>{label}</span>
          <Help schema={schema} />
        </label>
      );
    case "integer":
    case "number": {
      const range = [
        schema.minimum !== undefined ? `at least ${schema.minimum}` : null,
        schema.maximum !== undefined ? `at most ${schema.maximum}` : null,
      ].filter(Boolean);
      return (
        <label className="field" htmlFor={id}>
          <span>
            {label}
            {range.length > 0 && <small className="muted"> ({range.join(", ")})</small>}
          </span>
          <input
            id={id}
            type="number"
            required={required}
            min={schema.minimum}
            max={schema.maximum}
            step={schema.type === "integer" ? 1 : "any"}
            value={typeof value === "number" ? value : ""}
            onChange={(e) => onChange(parseNumber(e.target.value, schema.type === "integer"))}
          />
          <Help schema={schema} />
        </label>
      );
    }
    case "string": {
      if (schema.enum) {
        return (
          <label className="field" htmlFor={id}>
            <span>{label}</span>
            <select id={id} value={String(value ?? "")} onChange={(e) => onChange(e.target.value)}>
              {schema.enum.map((o) => (
                <option key={String(o)} value={String(o)}>
                  {String(o)}
                </option>
              ))}
            </select>
            <Help schema={schema} />
          </label>
        );
      }
      const common = {
        id,
        required,
        minLength: schema.minLength,
        maxLength: schema.maxLength,
        value: typeof value === "string" ? value : "",
      };
      return (
        <label className="field" htmlFor={id}>
          <span>{label}</span>
          {schema.format === "multiline" ? (
            <textarea {...common} rows={4} onChange={(e) => onChange(e.target.value)} />
          ) : (
            <input
              {...common}
              type={schema.format === "uri" ? "url" : "text"}
              spellCheck={false}
              onChange={(e) => onChange(e.target.value)}
            />
          )}
          <Help schema={schema} />
        </label>
      );
    }
    case "array":
      return <ArrayField name={name} schema={schema} value={value} onChange={onChange} />;
    case "object":
      return (
        <fieldset className="plugin-fieldset">
          <legend>{label}</legend>
          <Help schema={schema} />
          <ObjectFields schema={schema} value={(value as Values) ?? {}} onChange={onChange} />
        </fieldset>
      );
  }
}

function ArrayField({ name, schema, value, onChange }: Omit<FieldProps, "required">) {
  const items = Array.isArray(value) ? value : [];
  const itemSchema = schema.items!;
  const label = labelFor(name, schema);
  const full = schema.maxItems !== undefined && items.length >= schema.maxItems;

  // A list of fixed choices is a set of checkboxes.
  if (itemSchema.type === "string" && itemSchema.enum) {
    return (
      <fieldset className="plugin-fieldset">
        <legend>{label}</legend>
        {itemSchema.enum.map((o) => (
          <label key={String(o)} className="plugin-check">
            <input
              type="checkbox"
              checked={items.includes(o)}
              onChange={(e) => onChange(e.target.checked ? [...items, o] : items.filter((i) => i !== o))}
            />
            <span>{String(o)}</span>
          </label>
        ))}
        <Help schema={schema} />
      </fieldset>
    );
  }

  const set = (i: number, v: unknown) => onChange(items.map((old, j) => (j === i ? v : old)));
  return (
    <fieldset className="plugin-fieldset">
      <legend>{label}</legend>
      <Help schema={schema} />
      {items.length === 0 && <p className="muted">None yet.</p>}
      {items.map((item, i) => (
        <div key={i} className="plugin-array-item">
          <div className="plugin-array-body">
            <Field
              name={`item${i + 1}`}
              schema={{ ...itemSchema, title: `${itemSchema.title ?? label} #${i + 1}` }}
              value={item}
              required
              onChange={(v) => set(i, v)}
            />
          </div>
          <button type="button" className="link" onClick={() => onChange(items.filter((_, j) => j !== i))}>
            Remove
          </button>
        </div>
      ))}
      <div className="actions start">
        <button type="button" disabled={full} onClick={() => onChange([...items, emptyValue(itemSchema)])}>
          Add
        </button>
        {full && <small className="muted">At most {schema.maxItems}.</small>}
      </div>
    </fieldset>
  );
}

function ObjectFields({
  schema,
  value,
  onChange,
  skip = [],
}: {
  schema: JsonSchema;
  value: Values;
  onChange: (v: Values) => void;
  skip?: string[];
}) {
  const required = new Set(schema.required ?? []);
  return (
    <>
      {Object.entries(schema.properties ?? {})
        .filter(([k]) => !skip.includes(k))
        .map(([k, sub]) => (
          <Field
            key={k}
            name={k}
            schema={sub}
            value={value[k]}
            required={required.has(k)}
            onChange={(v) => onChange({ ...value, [k]: v })}
          />
        ))}
    </>
  );
}

function SecretField({
  name,
  schema,
  isSet,
  edit,
  onEdit,
}: {
  name: string;
  schema: JsonSchema;
  isSet: boolean;
  edit: SecretEdit;
  onEdit: (e: SecretEdit) => void;
}) {
  const id = useId();
  const state =
    edit.kind === "clear" ? "will be cleared" : edit.kind === "set" ? "will be replaced" : isSet ? "set" : "not set";
  return (
    <label className="field" htmlFor={id}>
      <span>
        {labelFor(name, schema)} <small className="muted">(secret, {state})</small>
      </span>
      <div className="field-row">
        <input
          id={id}
          type="password"
          autoComplete="new-password"
          placeholder={isSet ? "Leave empty to keep the current value" : ""}
          value={edit.kind === "set" ? edit.value : ""}
          onChange={(e) => onEdit(e.target.value ? { kind: "set", value: e.target.value } : { kind: "keep" })}
        />
        {isSet && (
          <button type="button" onClick={() => onEdit({ kind: "clear" })}>
            Clear
          </button>
        )}
      </div>
      <Help schema={schema} />
    </label>
  );
}

export function SchemaForm({
  schema,
  values,
  onChange,
  secrets,
  secretsSet,
  onSecret,
}: {
  schema: JsonSchema;
  values: Values;
  onChange: (v: Values) => void;
  secrets: Record<string, SecretEdit>;
  secretsSet: string[];
  onSecret: (name: string, edit: SecretEdit) => void;
}) {
  const secretNames = Object.keys(secrets);
  return (
    <div className="plugin-form">
      {secretNames.map((k) => (
        <SecretField
          key={k}
          name={k}
          schema={schema.properties![k]!}
          isSet={secretsSet.includes(k)}
          edit={secrets[k]!}
          onEdit={(e) => onSecret(k, e)}
        />
      ))}
      <ObjectFields schema={schema} value={values} onChange={onChange} skip={secretNames} />
    </div>
  );
}
