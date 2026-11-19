import { useState } from 'react';
import { call, errMsg } from '../api';
import { useI18n } from '../i18n';
import { useStore } from '../store';
import { Icon } from './Icon';
import { Modal } from './ui';

export function RepairDialog() {
  const { t } = useI18n();
  const offer = useStore((s) => s.repairOffer);
  const instances = useStore((s) => s.instances);
  const dismiss = useStore((s) => s.dismissRepair);
  const go = useStore((s) => s.go);
  const select = useStore((s) => s.select);
  const install = useStore((s) => s.install);
  const toast = useStore((s) => s.toast);
  const [depsBusy, setDepsBusy] = useState(false);

  if (!offer) return null;
  const inst = instances.find((i) => i.id === offer.instanceId);
  const name = inst?.name ?? offer.instanceId;
  const busy = inst?.running || inst?.state === 'installing' || depsBusy;

  const reasonKey =
    offer.reason === 'missing_mod'
      ? 'repair.reason.missing_mod'
      : offer.reason === 'incompatibility'
        ? 'repair.reason.incompatibility'
        : 'repair.reason.crash';

  const reinstall = () => {
    dismiss();
    void install(offer.instanceId);
  };

  const restore = async () => {
    dismiss();
    try {
      await call('instances:repairContent', { id: offer.instanceId });
    } catch (e) {
      toast('error', errMsg(e));
    }
  };

  const installMissingDeps = async () => {
    if (!inst || offer.missingDeps.length === 0) return;
    setDepsBusy(true);
    const loader = inst.loader.type === 'vanilla' ? null : inst.loader.type;
    const installed: string[] = [];
    const failed: string[] = [];
    try {
      for (const dep of offer.missingDeps) {
        try {
          const res = await call('modrinth:search', {
            query: dep,
            type: 'mod',
            mcVersion: inst.mcVersion && inst.mcVersion !== '?' ? inst.mcVersion : null,
            loader: loader === 'fabric' || loader === 'quilt' || loader === 'forge' || loader === 'neoforge' ? loader : null,
            sort: 'relevance',
            offset: 0,
          });
          const hit = res.hits.find((h) => h.slug === dep || h.id === dep || h.slug.toLowerCase() === dep.toLowerCase())
            ?? res.hits.find((h) => h.title.toLowerCase() === dep.toLowerCase())
            ?? res.hits[0];
          if (!hit) {
            failed.push(dep);
            continue;
          }
          await call('modrinth:install', {
            id: inst.id,
            projectId: hit.id,
            kind: 'mod',
          });
          installed.push(hit.title || dep);
        } catch {
          failed.push(dep);
        }
      }
      if (installed.length > 0) toast('ok', `${t('repair.depsInstalled')}: ${installed.join(', ')}`);
      if (failed.length > 0) toast('error', `${t('repair.depsFailed')}: ${failed.join(', ')}`);
      if (installed.length > 0 && failed.length === 0) dismiss();
    } finally {
      setDepsBusy(false);
    }
  };

  const openLogs = () => {
    select(offer.instanceId);
    dismiss();
    go('console', { consoleTab: 'game' });
  };

  return (
    <Modal
      title={t('repair.title')}
      onClose={depsBusy ? () => undefined : dismiss}
      footer={
        <>
          <button className="btn" onClick={dismiss} disabled={depsBusy}>{t('repair.close')}</button>
          <button className="btn" onClick={openLogs} disabled={depsBusy}><Icon name="terminal" size={16} /> {t('repair.logs')}</button>
        </>
      }
    >
      <div className="stack" style={{ gap: 12 }}>
        <p><strong>{name}</strong></p>
        <p>{t(reasonKey)}</p>
        {offer.details.length > 0 && (
          <div className="card" style={{ padding: 12 }}>
            <div style={{ marginBottom: 8 }}><strong>{t('repair.details')}</strong></div>
            <ul style={{ margin: 0, paddingLeft: 18, display: 'grid', gap: 4 }}>
              {offer.details.map((line, i) => (
                <li key={i} className="mono" style={{ fontSize: 12, wordBreak: 'break-word' }}>{line}</li>
              ))}
            </ul>
          </div>
        )}
        <p className="muted">{t('repair.hint')}</p>
        {offer.missingDeps.length > 0 && (
          <div className="card" style={{ padding: 12 }}>
            <div className="row" style={{ justifyContent: 'space-between', gap: 12, alignItems: 'center' }}>
              <div style={{ minWidth: 0 }}>
                <div><strong>{t('repair.installDeps')}</strong></div>
                <div className="muted" style={{ fontSize: 13 }}>
                  {t('repair.installDepsHint')}: <span className="mono">{offer.missingDeps.join(', ')}</span>
                </div>
              </div>
              <button className="btn btn--primary" onClick={() => void installMissingDeps()} disabled={busy || !inst || inst.mcVersion === '?'}>
                <Icon name="download" size={16} /> {depsBusy ? t('repair.installingDeps') : t('repair.installDeps')}
              </button>
            </div>
          </div>
        )}
        <div className="card" style={{ padding: 12 }}>
          <div className="row" style={{ justifyContent: 'space-between', gap: 12, alignItems: 'center' }}>
            <div style={{ minWidth: 0 }}>
              <div><strong>{t('repair.reinstall')}</strong></div>
              <div className="muted" style={{ fontSize: 13 }}>{t('repair.reinstallHint')}</div>
            </div>
            <button className="btn btn--primary" onClick={reinstall} disabled={busy}>
              <Icon name="download" size={16} /> {t('action.install')}
            </button>
          </div>
        </div>
        <div className="card" style={{ padding: 12 }}>
          <div className="row" style={{ justifyContent: 'space-between', gap: 12, alignItems: 'center' }}>
            <div style={{ minWidth: 0 }}>
              <div><strong>{t('repair.content')}</strong></div>
              <div className="muted" style={{ fontSize: 13 }}>{t('repair.contentHint')}</div>
            </div>
            <button className="btn" onClick={() => void restore()} disabled={busy}>
              <Icon name="wrench" size={16} /> {t('repair.content')}
            </button>
          </div>
        </div>
      </div>
    </Modal>
  );
}
