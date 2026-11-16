import { useEffect, useRef } from "react";
import type { ReactNode } from "react";
import { X, HardDrive, CircleHelp } from "lucide-react";
export function Dialog({
  title,
  subtitle,
  children,
  onClose,
  wide = false,
  drawer = false,
  footer,
}: {
  title: string;
  subtitle?: string;
  children: ReactNode;
  onClose: () => void;
  wide?: boolean;
  drawer?: boolean;
  footer?: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const el = ref.current!;
    el.showModal();
    el.querySelector<HTMLInputElement>(
      'input:not([type="range"]):not([type="checkbox"]), select',
    )?.focus();
    return () => el.close();
  }, []);
  return (
    <dialog
      ref={ref}
      className={`dialog ${wide ? "wide" : ""} ${drawer ? "drawer" : ""} ${footer ? "with-footer" : ""}`}
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="dialog-inner">
        <header className="dialog-header">
          <div>
            <h2>{title}</h2>
            {subtitle && <p>{subtitle}</p>}
          </div>
          <button
            className="icon-button"
            aria-label="关闭面板"
            onClick={onClose}
          >
            <X size={19} />
          </button>
        </header>
        {footer ? <div className="dialog-body">{children}</div> : children}
        {footer && <footer className="dialog-footer">{footer}</footer>}
      </div>
    </dialog>
  );
}
export function DiskIcon({
  className = "",
  size = 28,
}: {
  className?: string;
  size?: number;
}) {
  return <HardDrive className={className} size={size} strokeWidth={1.65} />;
}
export function Empty({
  icon,
  title,
  text,
  children,
}: {
  icon: ReactNode;
  title: string;
  text: string;
  children?: ReactNode;
}) {
  return (
    <div className="empty">
      <span className="empty-icon">{icon}</span>
      <h3>{title}</h3>
      <p>{text}</p>
      {children}
    </div>
  );
}
export function Hint({ children }: { children: ReactNode }) {
  return (
    <div className="hint">
      <CircleHelp size={16} />
      <span>{children}</span>
    </div>
  );
}
export function FormError({ message }: { message: string }) {
  return message ? (
    <p role="alert" className="form-error">
      {message}
    </p>
  ) : null;
}
