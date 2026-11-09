// Creating or changing a rule, with a dry run of what it would take from the current
// candidates.

import { useState, type FormEvent } from "react";

import type { DryRun } from "../api/generated/DryRun";
import type { MatchMode } from "../api/generated/MatchMode";
import type { MetadataMatch } from "../api/generated/MetadataMatch";
import type { RuleSpec } from "../api/generated/RuleSpec";
import type { SeedLimits } from "../api/generated/SeedLimits";
import type { VersionField } from "../api/generated/VersionField";
import type { VersionScheme } from "../api/generated/VersionScheme";
import { describeError } from "../auth/flow";
import { NO_SEED_LIMITS, SeedLimitsFields } from "../components/SeedLimitsFields";
import { rulesApi } from "./api";
import { MATCH_MODES, SCHEMES, VERDICT_TEXT, lines, orNull, problems, textMatch } from "./ruleForm";

function ModeSelect({ value, onChange, label }: { value: MatchMode; onChange: (m: MatchMode) => void; label: string }) {
  return (
    <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value as MatchMode)}>
      {MATCH_MODES.map((m) => (
        <option key={m.mode} value={m.mode}>
          {m.text}
        </option>
      ))}
    </select>
  );
}

export function RuleEditor({
  id,
  initial,
  sources,
  onSaved,
  onCancel,
}: {
  /** The saved rule's id; null for a new one. */
  id: string | null;
  initial: RuleSpec;
  /** Sources that have offered candidates, to pick from. */
  sources: string[];
  onSaved: () => void;
  onCancel: () => void;
}) {
  const [rule, setRule] = useState<RuleSpec>(initial);
  const [sourcesText, setSourcesText] = useState(initial.match.sources.join("\n"));
  const [titleMode, setTitleMode] = useState<MatchMode>(initial.match.title?.mode ?? "contains");
  const [title, setTitle] = useState(initial.match.title?.pattern ?? "");
  const [excludeMode, setExcludeMode] = useState<MatchMode>(initial.match.excludeTitle?.mode ?? "contains");
  const [exclude, setExclude] = useState(initial.match.excludeTitle?.pattern ?? "");
  const [metadata, setMetadata] = useState<MetadataMatch[]>(initial.match.metadata);
  const [include, setInclude] = useState(initial.files.include.join("\n"));
  const [excludeFiles, setExcludeFiles] = useState(initial.files.exclude.join("\n"));
  const [limits, setLimits] = useState<SeedLimits>(
    initial.superseded.action === "seedLimits" ? initial.superseded.limits : { ...NO_SEED_LIMITS, ratio: 2 },
  );
  const [aliasOn, setAliasOn] = useState(initial.alias !== null);
  const [aliasPath, setAliasPath] = useState(initial.alias?.path ?? "");
  const [aliasFile, setAliasFile] = useState(initial.alias?.file ?? "");
  const [dry, setDry] = useState<DryRun | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  /** The rule as the server takes it. */
  function spec(): RuleSpec {
    return {
      ...rule,
      name: rule.name.trim(),
      match: {
        sources: lines(sourcesText),
        title: textMatch(titleMode, title),
        excludeTitle: textMatch(excludeMode, exclude),
        metadata: metadata.filter((m) => m.field.trim() || m.pattern.trim()),
      },
      label: orNull(rule.label ?? ""),
      savePath: orNull(rule.savePath ?? ""),
      files: { include: lines(include), exclude: lines(excludeFiles) },
      superseded: rule.superseded.action === "seedLimits" ? { action: "seedLimits", limits } : rule.superseded,
      alias: aliasOn ? { path: aliasPath.trim(), file: orNull(aliasFile) } : null,
    };
  }

  async function dryRun() {
    setError(null);
    try {
      setDry(await rulesApi.dryRun(spec(), id));
    } catch (e) {
      setError(describeError(e));
    }
  }

  async function save(e: FormEvent) {
    e.preventDefault();
    const s = spec();
    const wrong = problems(s);
    if (wrong.length) {
      setError(wrong.join(" "));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      if (id) await rulesApi.update(id, s);
      else await rulesApi.create(s);
      onSaved();
    } catch (err) {
      setError(describeError(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="account-section rule-editor" onSubmit={(e) => void save(e)}>
      <h2>{id ? `Change “${initial.name}”` : "New rule"}</h2>
      <label className="field">
        <span>Name</span>
        <input value={rule.name} required maxLength={200} onChange={(e) => setRule({ ...rule, name: e.target.value })} />
      </label>
      <label className="check">
        <input type="checkbox" checked={rule.enabled} onChange={(e) => setRule({ ...rule, enabled: e.target.checked })} />
        Enabled
      </label>

      <h3>Which releases</h3>
      <label className="field">
        <span>
          Sources, one per line <span className="muted">(empty: every source)</span>
        </span>
        <textarea rows={2} value={sourcesText} onChange={(e) => setSourcesText(e.target.value)} />
      </label>
      {sources.length > 0 && (
        <p className="muted rule-hint">
          Sources with candidates now:{" "}
          {sources.map((s, i) => (
            <span key={s}>
              {i > 0 && ", "}
              <button
                type="button"
                className="link"
                onClick={() => setSourcesText(lines(`${sourcesText}\n${s}`).filter((v, j, a) => a.indexOf(v) === j).join("\n"))}
              >
                {s}
              </button>
            </span>
          ))}
        </p>
      )}
      <div className="field-row rule-match">
        <span className="rule-match-label">Title</span>
        <ModeSelect label="How the title is matched" value={titleMode} onChange={setTitleMode} />
        <input aria-label="Title pattern" placeholder="anything" value={title} onChange={(e) => setTitle(e.target.value)} />
      </div>
      <div className="field-row rule-match">
        <span className="rule-match-label">But not if it</span>
        <ModeSelect label="How excluded titles are matched" value={excludeMode} onChange={setExcludeMode} />
        <input aria-label="Excluded title pattern" placeholder="nothing" value={exclude} onChange={(e) => setExclude(e.target.value)} />
      </div>
      {metadata.map((m, i) => (
        <div className="field-row rule-match" key={i}>
          <input
            aria-label="Metadata field"
            className="rule-field-name"
            placeholder="field (flavor)"
            value={m.field}
            onChange={(e) => setMetadata(metadata.map((x, j) => (j === i ? { ...x, field: e.target.value } : x)))}
          />
          <ModeSelect
            label="How the field is matched"
            value={m.mode}
            onChange={(mode) => setMetadata(metadata.map((x, j) => (j === i ? { ...x, mode } : x)))}
          />
          <input
            aria-label="Metadata pattern"
            value={m.pattern}
            onChange={(e) => setMetadata(metadata.map((x, j) => (j === i ? { ...x, pattern: e.target.value } : x)))}
          />
          <button type="button" aria-label="Remove this condition" onClick={() => setMetadata(metadata.filter((_, j) => j !== i))}>
            ✕
          </button>
        </div>
      ))}
      <button type="button" className="link" onClick={() => setMetadata([...metadata, { field: "", mode: "equals", pattern: "" }])}>
        + Metadata condition
      </button>

      <h3>Which are newest</h3>
      <p className="muted rule-hint">
        Releases whose version doesn't read under the chosen scheme are left out (the dry run says why). Feed items
        usually have no version: compare their publication dates, or their titles as text.
      </p>
      <div className="field-row">
        <label className="field grow">
          <span>Compare</span>
          <select
            value={rule.order.field}
            onChange={(e) => setRule({ ...rule, order: { ...rule.order, field: e.target.value as VersionField } })}
          >
            <option value="version">the version</option>
            <option value="title">the title</option>
            <option value="published">the publication date</option>
          </select>
        </label>
        <label className="field grow">
          <span>As</span>
          <select
            value={rule.order.scheme}
            onChange={(e) => setRule({ ...rule, order: { ...rule.order, scheme: e.target.value as VersionScheme } })}
          >
            {SCHEMES.map((s) => (
              <option key={s.scheme} value={s.scheme}>
                {s.text}
              </option>
            ))}
          </select>
        </label>
        <label className="field">
          <span>
            Keep the latest <span className="muted">(0: all)</span>
          </span>
          <input
            type="number"
            min={0}
            max={1000}
            value={rule.keepLatest}
            onChange={(e) => setRule({ ...rule, keepLatest: Math.max(0, Math.floor(Number(e.target.value) || 0)) })}
          />
        </label>
      </div>

      <h3>Where they go</h3>
      <div className="field-row">
        <label className="field grow">
          <span>Label</span>
          <input value={rule.label ?? ""} maxLength={100} onChange={(e) => setRule({ ...rule, label: e.target.value })} />
        </label>
        <label className="field grow">
          <span>
            Folder <span className="muted">(absolute, or inside the download directory)</span>
          </span>
          <input value={rule.savePath ?? ""} onChange={(e) => setRule({ ...rule, savePath: e.target.value })} />
        </label>
      </div>
      <label className="check">
        <input type="checkbox" checked={rule.paused} onChange={(e) => setRule({ ...rule, paused: e.target.checked })} />
        Add paused
      </label>
      <div className="field-row">
        <label className="field grow">
          <span>
            Download only files matching, one glob per line <span className="muted">(empty: all; *.iso matches the name in any folder)</span>
          </span>
          <textarea rows={2} value={include} onChange={(e) => setInclude(e.target.value)} />
        </label>
        <label className="field grow">
          <span>Except files matching</span>
          <textarea rows={2} value={excludeFiles} onChange={(e) => setExcludeFiles(e.target.value)} />
        </label>
      </div>

      <h3>When a newer release replaces one</h3>
      <label className="field">
        <span>Older releases</span>
        <select
          value={rule.superseded.action}
          onChange={(e) => {
            const action = e.target.value as RuleSpec["superseded"]["action"];
            setRule({ ...rule, superseded: action === "seedLimits" ? { action, limits } : { action } });
          }}
        >
          <option value="keepSeeding">Keep seeding</option>
          <option value="stop">Pause them</option>
          <option value="seedLimits">Seed until a ratio or a time, then…</option>
        </select>
      </label>
      {rule.superseded.action === "seedLimits" && <SeedLimitsFields value={limits} onChange={setLimits} idPrefix="rule-superseded" />}

      <h3>Stable “latest” path</h3>
      <label className="check">
        <input type="checkbox" checked={aliasOn} onChange={(e) => setAliasOn(e.target.checked)} />
        Keep a path that always names the newest completed release
      </label>
      {aliasOn && (
        <div className="field-row">
          <label className="field grow">
            <span>Path inside the download directory</span>
            <input placeholder="library/ubuntu/latest-desktop-amd64.iso" value={aliasPath} onChange={(e) => setAliasPath(e.target.value)} />
          </label>
          <label className="field grow">
            <span>
              Which file <span className="muted">(a glob; the largest match; empty: the largest file)</span>
            </span>
            <input placeholder="*.iso" value={aliasFile} onChange={(e) => setAliasFile(e.target.value)} />
          </label>
        </div>
      )}
      {aliasOn && (
        <p className="muted rule-hint">
          A hard link to the file (a copy if it's on another drive), replaced in one step when a newer release
          finishes. It stays valid after the old torrent is removed.
        </p>
      )}

      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      <div className="actions start">
        <button type="submit" className="primary" disabled={busy}>
          {id ? "Save" : "Create"}
        </button>
        <button type="button" disabled={busy} onClick={() => void dryRun()}>
          Dry run
        </button>
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
      </div>
      {dry && <DryRunTable dry={dry} keep={spec().keepLatest} />}
    </form>
  );
}

function DryRunTable({ dry, keep }: { dry: DryRun; keep: number }) {
  return (
    <div className="dry-run" aria-live="polite">
      <h3>Dry run</h3>
      <p className="muted">
        {dry.entries.length} of {dry.considered} candidates match; {keep === 0 ? "every one is kept" : `the latest ${keep} are kept`}.
        Nothing was changed.
      </p>
      {dry.entries.length > 0 && (
        <table className="list">
          <thead>
            <tr>
              <th>#</th>
              <th>Release</th>
              <th>Version</th>
              <th>What happens</th>
            </tr>
          </thead>
          <tbody>
            {dry.entries.map((e) => (
              <tr key={`${e.source}\n${e.candidateId}`} className={`verdict-${e.verdict}`}>
                <td>{e.rank ?? ""}</td>
                <td>
                  {e.title}
                  <div className="muted rule-source">{e.source}</div>
                </td>
                <td>{e.version ?? <span className="muted">none</span>}</td>
                <td>
                  {VERDICT_TEXT[e.verdict]}
                  {e.reason && <div className="muted">{e.reason}</div>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
