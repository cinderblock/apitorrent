import type { ReactNode } from "react";

/** The apitorrent mark: a download arrow into a tray on a rounded square (the app icon). */
export function BrandMark({ size = 28 }: { size?: number }) {
  return (
    <svg className="brand-mark" width={size} height={size} viewBox="64 64 896 896" aria-hidden="true">
      <rect x="64" y="64" width="896" height="896" rx="200" fill="currentColor" />
      <g fill="none" stroke="var(--on-accent)" strokeLinecap="round" strokeLinejoin="round">
        <path d="M512 220 V600" strokeWidth="96" />
        <path d="M340 440 L512 612 L684 440" strokeWidth="96" />
        <path d="M260 700 V780 H764 V700" strokeWidth="80" />
      </g>
    </svg>
  );
}

interface Props {
  /** The page's own heading; without one only the brand shows. */
  title?: string;
  /** A sentence under the title saying what the page is for. */
  lead?: ReactNode;
}

/** The top of every sign-in, setup and connect card. */
export function AuthHeader({ title, lead }: Props) {
  return (
    <header className="auth-header">
      <div className="auth-brand">
        <BrandMark />
        <span>apitorrent</span>
      </div>
      {title && <h1>{title}</h1>}
      {lead && <p className="auth-lead">{lead}</p>}
    </header>
  );
}
