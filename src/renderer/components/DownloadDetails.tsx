import { useEffect } from 'react';
import type { OpFileEntry } from '../../shared/types';
import { useI18n } from '../i18n';
import { useStore } from '../store';
import { formatSize } from '../util';
import { Icon } from './Icon';
import { Modal, ProgressBar } from './ui';

type T = (key: string, vars?: Record<string, string | number>) => string;

function formatSpeed(bytesPerSecond: number, t: T): string {
  return t('download.speed', { v: formatSize(Math.round(bytesPerSecond), t) });
}

function formatEta(seconds: number, t: T): string {
  if (seconds < 60) return t('download.seconds', { n: Math.max(1, Math.round(seconds)) });
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return t('time.m', { m: minutes });
  return t('time.hm', { h: Math.floor(minutes / 60), m: minutes % 60 });
}

function FileRow({ file, t }: { file: OpFileEntry; t: T }) {
  const fraction = file.total && file.total > 0 ? Math.min(1, file.done / file.total) : null;
  return (
    <li className="dl-file">
      <div className="dl-file__head">
        <span className="dl-file__name" title={file.name}>{file.name}</span>
        <span className="dl-file__size">
          {file.total ? `${formatSize(file.done, t)} / ${formatSize(file.total, t)}` : formatSize(file.done, t)}
        </span>
      </div>
      <ProgressBar fraction={fraction} />
      <small className="dl-file__purpose">{file.purpose}</small>
    </li>
  );
}

/** The «Подробнее» panel: overall progress of the running operation and the files being downloaded right now. */
export function DownloadDetails() {
  const { t } = useI18n();
  const opId = useStore((s) => s.detailsOpId);
  const op = useStore((s) => (s.detailsOpId ? s.ops[s.detailsOpId] : undefined));
  const files = useStore((s) => (s.detailsOpId ? s.opFiles[s.detailsOpId] : undefined));
  const close = useStore((s) => s.openDetails);

  // the operation is gone (finished and cleaned up): nothing left to show
  useEffect(() => {
    if (opId && !op && !files) close(null);
  }, [opId, op, files, close]);

  if (!opId || (!op && !files)) return null;

  const running = !op || op.status === 'running';
  const fraction = op ? op.fraction : null;
  const bytesKnown = !!files && files.bytesTotal > 0;
  const remaining = files && files.bytesTotal > files.bytesDone ? files.bytesTotal - files.bytesDone : 0;
  const eta = running && files && files.speed > 1024 && remaining > 0 ? remaining / files.speed : null;

  return (
    <Modal title={t('download.title')} onClose={() => close(null)} wide>
      <div className="dl">
        <section className="dl-total">
          <div className="dl-total__head">
            <strong className="dl-total__label" title={op?.label}>{op?.label ?? t('download.title')}</strong>
            {fraction !== null && <span className="dl-total__pct">{Math.round(fraction * 100)}%</span>}
          </div>
          <ProgressBar fraction={running ? fraction : 1} />
          {files && (
            <dl className="dl-stats">
              <div>
                <dt>{t('download.files')}</dt>
                <dd>{files.filesDone} / {files.filesTotal}</dd>
              </div>
              {bytesKnown && (
                <div>
                  <dt>{t('download.size')}</dt>
                  <dd>{formatSize(files.bytesDone, t)} / {formatSize(files.bytesTotal, t)}</dd>
                </div>
              )}
              <div>
                <dt>{t('download.speedLabel')}</dt>
                <dd>{running && files.speed > 0 ? formatSpeed(files.speed, t) : '—'}</dd>
              </div>
              <div>
                <dt>{t('download.eta')}</dt>
                <dd>{eta !== null ? formatEta(eta, t) : '—'}</dd>
              </div>
            </dl>
          )}
          {op?.status === 'error' && op.message && <p className="dl-error">{op.message}</p>}
        </section>

        <section className="dl-list">
          <h3>
            <Icon name="download" size={16} /> {t('download.current')}
            {files && files.active.length > 0 && <span className="dl-list__count">{files.active.length}</span>}
          </h3>
          {files && files.active.length > 0 ? (
            <ul className="dl-files">
              {files.active.map((file) => <FileRow key={file.id} file={file} t={t} />)}
            </ul>
          ) : (
            <p className="dl-empty">{running ? t('download.waiting') : t('download.finished')}</p>
          )}
        </section>
      </div>
    </Modal>
  );
}
