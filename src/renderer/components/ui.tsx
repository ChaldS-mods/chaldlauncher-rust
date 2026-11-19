import { Children, isValidElement, useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent as ReactKeyEvent, type ReactElement, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import type { InstanceView } from '../../shared/types';
import { useI18n } from '../i18n';
import { COVER_COLORS } from '../util';
import { Icon } from './Icon';

export function Modal({ title, onClose, children, footer, wide }: { title: string; onClose: () => void; children: ReactNode; footer?: ReactNode; wide?: boolean }) {
  const { t } = useI18n();
  useEffect(() => {
    const h = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', h);
    return () => window.removeEventListener('keydown', h);
  }, [onClose]);
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className={`modal${wide ? ' modal--wide' : ''}`} role="dialog" aria-modal="true" aria-label={title}>
        <header className="modal__head">
          <h2>{title}</h2>
          <button className="icon-btn" onClick={onClose} aria-label={t('action.close')}><Icon name="x" /></button>
        </header>
        <div className="modal__body">{children}</div>
        {footer && <footer className="modal__foot">{footer}</footer>}
      </div>
    </div>
  );
}

export function ConfirmModal(p: { title: string; children: ReactNode; confirmLabel: string; danger?: boolean; busy?: boolean; onConfirm: () => void; onClose: () => void }) {
  const { t } = useI18n();
  return (
    <Modal
      title={p.title}
      onClose={p.onClose}
      footer={
        <>
          <button className="btn" onClick={p.onClose}>{t('action.cancel')}</button>
          <button className={`btn ${p.danger ? 'btn--danger' : 'btn--primary'}`} onClick={p.onConfirm} disabled={p.busy}>{p.confirmLabel}</button>
        </>
      }
    >
      {p.children}
    </Modal>
  );
}

export interface MenuItem { label: string; icon: string; onClick: () => void; danger?: boolean; disabled?: boolean }

export function Menu({ items }: { items: MenuItem[] }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const { t } = useI18n();
  useEffect(() => {
    if (!open) return;
    const h = (e: MouseEvent) => ref.current && !ref.current.contains(e.target as Node) && setOpen(false);
    window.addEventListener('mousedown', h);
    return () => window.removeEventListener('mousedown', h);
  }, [open]);
  return (
    <div className="menu" ref={ref}>
      <button className="icon-btn" aria-label={t('action.edit')} onClick={(e) => { e.stopPropagation(); setOpen((o) => !o); }}><Icon name="dots" /></button>
      {open && (
        <ul className="menu__list" role="menu">
          {items.map((it) => (
            <li key={it.label}>
              <button role="menuitem" className={it.danger ? 'is-danger' : ''} disabled={it.disabled} onClick={(e) => { e.stopPropagation(); setOpen(false); it.onClick(); }}>
                <Icon name={it.icon} size={16} /> {it.label}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export function Cover({ inst, className = '' }: { inst: Pick<InstanceView, 'coverDataUrl' | 'coverColor'>; className?: string }) {
  if (inst.coverDataUrl) return <div className={`cover ${className}`}><img src={inst.coverDataUrl} alt="" /></div>;
  const [a, b] = COVER_COLORS[inst.coverColor % COVER_COLORS.length]!;
  return (
    <div className={`cover cover--gen ${className}`} style={{ background: `linear-gradient(135deg, ${a}, ${b})` }}>
      <svg viewBox="0 0 48 48" fill="none" stroke="rgba(255,255,255,.55)" strokeWidth="1.6" strokeLinejoin="round" aria-hidden="true">
        <path d="M24 6l15 8.5v17L24 40 9 31.5v-17L24 6zM24 23l15-8.5M24 23L9 14.5M24 23v17" />
      </svg>
    </div>
  );
}

export function ProgressBar({ fraction }: { fraction: number | null }) {
  return (
    <div className={`progress${fraction === null ? ' progress--indeterminate' : ''}`} role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={fraction === null ? undefined : Math.round(fraction * 100)}>
      <div className="progress__bar" style={fraction === null ? undefined : { width: `${Math.round(fraction * 100)}%` }} />
    </div>
  );
}

export function Field({ label, hint, error, children }: { label: string; hint?: string; error?: string | null; children: ReactNode }) {
  return (
    <label className="field">
      <span className="field__label">{label}</span>
      {children}
      {error ? <span className="field__error">{error}</span> : hint ? <span className="field__hint">{hint}</span> : null}
    </label>
  );
}

export function Switch({ checked, onChange, label, disabled }: { checked: boolean; onChange: (v: boolean) => void; label: string; disabled?: boolean }) {
  return (
    <button type="button" role="switch" aria-checked={checked} aria-label={label} disabled={disabled} className={`switch${checked ? ' is-on' : ''}`} onClick={() => onChange(!checked)}>
      <span />
    </button>
  );
}

export function Empty({ icon, title, text, action }: { icon: string; title: string; text?: string; action?: ReactNode }) {
  return (
    <div className="empty">
      <div className="empty__icon"><Icon name={icon} size={34} /></div>
      <h3>{title}</h3>
      {text && <p>{text}</p>}
      {action}
    </div>
  );
}

export function Skeleton({ rows = 3 }: { rows?: number }) {
  return <div className="skeletons">{Array.from({ length: rows }, (_, i) => <div key={i} className="skeleton" />)}</div>;
}


// ── Select: замена нативного <select> (нативный список в WebView выглядит чужеродно и глючит) ──
function nodeText(node: ReactNode): string {
  if (node === null || node === undefined || typeof node === 'boolean') return '';
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join('');
  if (isValidElement(node)) return nodeText((node.props as { children?: ReactNode }).children);
  return '';
}

interface SelectOption { value: string; label: string; disabled: boolean }

function collectOptions(children: ReactNode, out: SelectOption[] = []): SelectOption[] {
  Children.forEach(children, (child) => {
    if (!isValidElement(child)) return;
    const el = child as ReactElement<{ value?: string | number; disabled?: boolean; children?: ReactNode }>;
    if (el.type === 'option') {
      const label = nodeText(el.props.children);
      out.push({ value: el.props.value !== undefined ? String(el.props.value) : label, label, disabled: !!el.props.disabled });
    } else if (el.props.children) {
      collectOptions(el.props.children, out);
    }
  });
  return out;
}

export interface SelectProps {
  value: string | number;
  onChange: (e: { target: { value: string } }) => void;
  children: ReactNode;
  className?: string;
  disabled?: boolean;
  id?: string;
  'aria-label'?: string;
}

export function Select({ value, onChange, children, className = '', disabled, id, 'aria-label': ariaLabel }: SelectProps) {
  const options = collectOptions(children);
  const current = String(value ?? '');
  const selected = options.find((o) => o.value === current);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [pos, setPos] = useState<{ left: number; top: number; width: number; maxH: number; up: boolean } | null>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const listId = useId();

  const place = useCallback(() => {
    const el = btnRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const below = window.innerHeight - r.bottom - 12;
    const above = r.top - 12;
    const up = below < 180 && above > below;
    setPos({ left: r.left, top: up ? r.top - 6 : r.bottom + 6, width: r.width, maxH: Math.max(120, Math.min(320, up ? above : below)), up });
  }, []);

  useLayoutEffect(() => { if (open) place(); }, [open, place]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (btnRef.current?.contains(t) || listRef.current?.contains(t)) return;
      setOpen(false);
    };
    const onMove = (e: Event) => { if (!listRef.current?.contains(e.target as Node)) setOpen(false); };
    document.addEventListener('mousedown', onDown);
    window.addEventListener('resize', onMove);
    window.addEventListener('scroll', onMove, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('resize', onMove);
      window.removeEventListener('scroll', onMove, true);
    };
  }, [open]);

  useEffect(() => {
    if (open) listRef.current?.querySelector<HTMLElement>('[data-active="true"]')?.scrollIntoView({ block: 'nearest' });
  }, [open, active]);

  const openList = () => {
    if (disabled) return;
    const i = options.findIndex((o) => o.value === current);
    setActive(i >= 0 ? i : Math.max(0, options.findIndex((o) => !o.disabled)));
    setOpen(true);
  };
  const choose = (o: SelectOption) => {
    if (o.disabled) return;
    setOpen(false);
    btnRef.current?.focus();
    if (o.value !== current) onChange({ target: { value: o.value } });
  };
  const step = (dir: 1 | -1) => {
    let i = active;
    for (let n = 0; n < options.length; n++) {
      i = (i + dir + options.length) % options.length;
      if (!options[i]?.disabled) break;
    }
    setActive(i);
  };
  const onKey = (e: ReactKeyEvent<HTMLButtonElement>) => {
    if (disabled) return;
    if (!open) {
      if (['ArrowDown', 'ArrowUp', 'Enter', ' '].includes(e.key)) { e.preventDefault(); openList(); }
      return;
    }
    if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); setOpen(false); }
    else if (e.key === 'ArrowDown') { e.preventDefault(); step(1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); step(-1); }
    else if (e.key === 'Home') { e.preventDefault(); setActive(0); }
    else if (e.key === 'End') { e.preventDefault(); setActive(options.length - 1); }
    else if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); const o = options[active]; if (o) choose(o); }
    else if (e.key === 'Tab') setOpen(false);
  };

  return (
    <>
      <button
        ref={btnRef}
        id={id}
        type="button"
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={listId}
        aria-label={ariaLabel}
        disabled={disabled}
        className={`input select ${open ? 'is-open' : ''} ${className}`}
        onClick={() => (open ? setOpen(false) : openList())}
        onKeyDown={onKey}
      >
        <span className="select__value">{selected?.label ?? ''}</span>
        <span className="select__chev" aria-hidden="true" />
      </button>
      {open && pos && createPortal(
        <ul
          ref={listRef}
          id={listId}
          role="listbox"
          className="select__list"
          style={{ left: pos.left, top: pos.top, minWidth: pos.width, maxHeight: pos.maxH, transform: pos.up ? 'translateY(-100%)' : undefined }}
        >
          {options.map((o, i) => (
            <li
              key={`${o.value}-${i}`}
              role="option"
              aria-selected={o.value === current}
              aria-disabled={o.disabled || undefined}
              data-active={i === active}
              className={`select__opt${o.value === current ? ' is-selected' : ''}${o.disabled ? ' is-disabled' : ''}`}
              onMouseEnter={() => !o.disabled && setActive(i)}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => choose(o)}
            >
              <span>{o.label}</span>
              {o.value === current && <Icon name="check" size={14} />}
            </li>
          ))}
        </ul>,
        document.body,
      )}
    </>
  );
}
