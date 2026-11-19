import { useCallback, useEffect, useMemo, useState } from 'react';
import { useBack } from '../back';
import { writeClipboard } from '../clipboard';
import { getT, useLang } from '../i18n';
import { errorToast, hostStatus, navigate, onMemoryChanged, toast, useStore } from '../store';
import { rpc } from '../transport';
import type {
  MemoryEntry,
  MemoryImportCandidate,
  MemoryListResponse,
  MemoryMeta,
  MemoryMode,
  MemoryPreviewResponse,
  MemoryProjectInfo,
  MemoryScope,
  PendingMemory,
} from '../types';
import { shortPath } from '../util';
import { Icon } from './Icon';
import { askConfirm, openMenu } from './Overlay';

const t = getT('memory');

function fmtBytes(n: number): string {
  if (n < 1024) return t('bytes', { n });
  return t('kb', { n: (n / 1024).toFixed(1) });
}

function slugifyId(raw: string): string {
  const s = raw
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 48);
  if (!s || s === 'project') {
    let h = 2166136261;
    for (let i = 0; i < raw.length; i++) {
      h ^= raw.charCodeAt(i);
      h = Math.imul(h, 16777619);
    }
    return `note-${(h >>> 0).toString(16).padStart(8, '0')}`;
  }
  return s;
}

export interface MemoryEditorInit {
  scope: MemoryScope;
  projectKey?: string | null;
  cwd?: string;
  id?: string;
  prevId?: string;
  title?: string;
  description?: string;
  mode?: MemoryMode;
  tags?: string[];
  content?: string;
  pendingId?: string;
}

export function MemoryEditorModal({
  hostId,
  kind,
  initial,
  projects,
  resolvedProject,
  cwd,
  onSaved,
  onClose,
}: {
  hostId: string;
  kind: 'create' | 'edit' | 'split' | 'review';
  initial: MemoryEditorInit;
  projects: MemoryProjectInfo[];
  resolvedProject?: MemoryProjectInfo | null;
  cwd?: string;
  onSaved?: () => void;
  onClose: () => void;
}) {
  useLang();
  useBack(true, onClose);
  const [scope, setScope] = useState<MemoryScope>(initial.scope);
  const projectChoices = useMemo(() => {
    const map = new Map<string, MemoryProjectInfo>();
    if (resolvedProject) map.set(resolvedProject.key, resolvedProject);
    for (const p of projects) map.set(p.key, p);
    return [...map.values()];
  }, [projects, resolvedProject]);
  const [projectKey, setProjectKey] = useState<string>(
    initial.projectKey ?? resolvedProject?.key ?? projectChoices[0]?.key ?? '',
  );
  const [title, setTitle] = useState(initial.title ?? '');
  const [id, setId] = useState(initial.id ?? '');
  const [idTouched, setIdTouched] = useState(kind === 'edit' || !!initial.id);
  const [mode, setMode] = useState<MemoryMode>(initial.mode ?? 'on-demand');
  const [description, setDescription] = useState(initial.description ?? '');
  const [tagsText, setTagsText] = useState((initial.tags ?? []).join(', '));
  const [content, setContent] = useState(initial.content ?? '');
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const onTitleChange = (v: string) => {
    setTitle(v);
    if (!idTouched && v.trim()) {
      setId(slugifyId(v.trim()));
    }
  };

  const canProject = projectChoices.length > 0 || !!cwd;
  const heading =
    kind === 'edit'
      ? t('editorEdit')
      : kind === 'split'
        ? t('editorSplit')
        : kind === 'review'
          ? t('editorReview')
          : t('editorCreate');

  const submit = async () => {
    const cleanId = id.trim() || (title.trim() ? slugifyId(title.trim()) : '');
    if (!cleanId) return;
    setSaving(true);
    try {
      const tags = tagsText
        .split(',')
        .map((x) => x.trim())
        .filter(Boolean);
      const effectiveCwd = scope === 'project' && (!projectKey || projectKey === resolvedProject?.key) ? cwd : undefined;
      if (kind === 'review' && initial.pendingId) {
        await rpc(hostId, 'memory.resolvePending', {
          pendingId: initial.pendingId,
          approve: true,
          scope,
          projectKey: scope === 'project' ? projectKey || undefined : undefined,
          id: cleanId,
          title: title.trim() || cleanId,
          description: description.trim(),
          mode,
          tags,
          content,
        });
        toast(t('approved'));
      } else {
        await rpc(hostId, 'memory.save', {
          scope,
          projectKey: scope === 'project' ? projectKey || undefined : undefined,
          cwd: effectiveCwd,
          id: cleanId,
          prevId: kind === 'edit' ? initial.prevId : undefined,
          title: title.trim() || cleanId,
          description: description.trim(),
          mode,
          tags,
          content,
        });
        toast(t('saved'));
      }
      onSaved?.();
      onClose();
    } catch (e) {
      errorToast(e);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal mem-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>{heading}</h3>
          <button className="icon-btn" onClick={onClose} aria-label={t('close')}>
            <Icon name="x" size={16} />
          </button>
        </div>
        <div className="mem-modal-body form">
          <div className="mem-form-grid">
            <label>
              {t('fieldScope')}
              <select
                value={scope}
                onChange={(e) => setScope(e.target.value as MemoryScope)}
                disabled={kind === 'edit'}
              >
                <option value="global">{t('globalScope')}</option>
                <option value="project" disabled={!canProject}>
                  {t('projectScope')}
                </option>
              </select>
            </label>
            {scope === 'project' && projectChoices.length > 0 && (
              <label>
                {t('fieldProject')}
                <select
                  value={projectKey}
                  onChange={(e) => setProjectKey(e.target.value)}
                  disabled={kind === 'edit'}
                >
                  {projectChoices.map((p) => (
                    <option key={p.key} value={p.key}>
                      {p.displayName}
                    </option>
                  ))}
                </select>
              </label>
            )}
            <label>
              {t('fieldMode')}
              <select value={mode} onChange={(e) => setMode(e.target.value as MemoryMode)}>
                <option value="on-demand">{t('modeOnDemand')}</option>
                <option value="always">{t('modeAlways')}</option>
              </select>
            </label>
          </div>
          <p className="muted small">
            {mode === 'always' ? t('modeAlwaysHint') : t('modeOnDemandHint')}
          </p>
          <div className="mem-form-grid">
            <label>
              {t('fieldTitle')}
              <input
                value={title}
                onChange={(e) => onTitleChange(e.target.value)}
                placeholder={t('fieldTitlePlaceholder')}
                autoFocus
              />
            </label>
            <label>
              {t('fieldId')}
              <input
                value={id}
                onChange={(e) => {
                  setIdTouched(true);
                  setId(e.target.value);
                }}
                placeholder={t('fieldIdPlaceholder')}
              />
            </label>
          </div>
          <label>
            {t('fieldDescription')}
            <input
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder={t('fieldDescriptionPlaceholder')}
            />
          </label>
          <label>
            {t('fieldTags')}
            <input
              value={tagsText}
              onChange={(e) => setTagsText(e.target.value)}
              placeholder={t('fieldTagsPlaceholder')}
            />
          </label>
          <label>
            {t('fieldContent')}
            <textarea
              className="mem-editor-ta"
              rows={11}
              value={content}
              onChange={(e) => setContent(e.target.value)}
              placeholder={t('fieldContentPlaceholder')}
            />
          </label>
          {mode === 'always' && content.length > 300 && (
            <p className="mem-warn small">{t('alwaysLengthWarn', { chars: content.length })}</p>
          )}
        </div>
        <div className="mem-modal-foot">
          <button className="btn" onClick={onClose}>
            {t('cancel')}
          </button>
          <button
            className="btn primary"
            disabled={saving || !content.trim() || (!id.trim() && !title.trim())}
            onClick={submit}
          >
            {saving ? t('saving') : kind === 'review' ? t('approve') : t('save')}
          </button>
        </div>
      </div>
    </div>
  );
}

export function MemoryPreviewModal({
  hostId,
  cwd,
  projectKey,
  onClose,
}: {
  hostId: string;
  cwd?: string;
  projectKey?: string;
  onClose: () => void;
}) {
  useLang();
  useBack(true, onClose);
  const [data, setData] = useState<MemoryPreviewResponse | null>(null);

  useEffect(() => {
    rpc<MemoryPreviewResponse>(hostId, 'memory.preview', { cwd, projectKey })
      .then(setData)
      .catch(errorToast);
  }, [hostId, cwd, projectKey]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal mem-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>{t('previewTitle')}</h3>
          <button className="icon-btn" onClick={onClose} aria-label={t('close')}>
            <Icon name="x" size={16} />
          </button>
        </div>
        <div className="mem-modal-body">
          {!data ? (
            <div className="muted small">{t('loading')}</div>
          ) : !data.prompt ? (
            <div className="muted small">{t('previewEmpty')}</div>
          ) : (
            <>
              <div className="muted small" style={{ marginBottom: 8 }}>
                {t('previewStats', {
                  always: data.alwaysCount,
                  index: data.indexCount,
                  omitted: data.omittedCount,
                  bytes: data.bytes,
                })}
              </div>
              <pre className="mem-preview-pre">{data.prompt}</pre>
            </>
          )}
        </div>
        <div className="mem-modal-foot">
          {data?.prompt && (
            <button
              className="btn"
              onClick={async () => {
                if (await writeClipboard(data.prompt)) toast(t('copied'));
              }}
            >
              <Icon name="copy" size={14} /> {t('copyPrompt')}
            </button>
          )}
          <button className="btn primary" onClick={onClose}>
            {t('close')}
          </button>
        </div>
      </div>
    </div>
  );
}

interface ImportRow {
  candidate: MemoryImportCandidate;
  selected: boolean;
  scope: MemoryScope;
  mode: MemoryMode;
  id: string;
}

export function MemoryImportModal({
  hostId,
  cwd,
  projectKey,
  onImported,
  onClose,
}: {
  hostId: string;
  cwd?: string;
  projectKey?: string;
  onImported?: () => void;
  onClose: () => void;
}) {
  useLang();
  useBack(true, onClose);
  const [loading, setLoading] = useState(true);
  const [rows, setRows] = useState<ImportRow[]>([]);
  const [overwrite, setOverwrite] = useState(false);
  const [importing, setImporting] = useState(false);

  useEffect(() => {
    setLoading(true);
    rpc<{ candidates: MemoryImportCandidate[] }>(hostId, 'memory.scanImport', { cwd })
      .then((res) => {
        setRows(
          (res.candidates ?? []).map((c) => ({
            candidate: c,
            selected: !c.alreadyExists,
            scope: c.suggestedScope,
            mode: c.suggestedMode,
            id: c.suggestedId,
          })),
        );
      })
      .catch(errorToast)
      .finally(() => setLoading(false));
  }, [hostId, cwd]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const selectedCount = rows.filter((r) => r.selected).length;

  const doImport = async () => {
    const items = rows
      .filter((r) => r.selected)
      .map((r) => ({
        scope: r.scope,
        projectKey: r.scope === 'project' ? projectKey : undefined,
        cwd: r.scope === 'project' ? cwd : undefined,
        id: r.id.trim() || r.candidate.suggestedId,
        title: r.candidate.title,
        description: r.candidate.description,
        mode: r.mode,
        content: r.candidate.content,
        overwrite,
      }));
    if (!items.length) return;
    setImporting(true);
    try {
      const res = await rpc<{ imported: number; skipped: number }>(hostId, 'memory.import', { items });
      toast(t('importDone', { imported: res.imported, skipped: res.skipped }));
      onImported?.();
      onClose();
    } catch (e) {
      errorToast(e);
    } finally {
      setImporting(false);
    }
  };

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal mem-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>{t('importTitle')}</h3>
          <button className="icon-btn" onClick={onClose} aria-label={t('close')}>
            <Icon name="x" size={16} />
          </button>
        </div>
        <div className="mem-modal-body">
          <p className="muted small" style={{ marginBottom: 10 }}>
            {t('importDesc')}
          </p>
          {loading ? (
            <div className="muted small">{t('importScanning')}</div>
          ) : rows.length === 0 ? (
            <div className="muted small">{t('importEmpty')}</div>
          ) : (
            <>
              <label className="check small" style={{ marginBottom: 10 }}>
                <input
                  type="checkbox"
                  checked={overwrite}
                  onChange={(e) => setOverwrite(e.target.checked)}
                />
                {t('importOverwrite')}
              </label>
              <div className="mem-import-list">
                {rows.map((r, idx) => (
                  <div key={r.candidate.path} className="mem-import-item">
                    <div className="mem-import-head">
                      <label className="check grow">
                        <input
                          type="checkbox"
                          checked={r.selected}
                          onChange={(e) =>
                            setRows((prev) =>
                              prev.map((x, i) => (i === idx ? { ...x, selected: e.target.checked } : x)),
                            )
                          }
                        />
                        <strong>{r.candidate.title}</strong>
                      </label>
                      {r.candidate.alreadyExists && (
                        <span className="acc-badge acc-badge-warn">{t('importExists')}</span>
                      )}
                      <span className="acc-badge acc-badge-muted">
                        {t('sourceLabel', { source: r.candidate.source })}
                      </span>
                    </div>
                    <div className="muted small truncate" title={r.candidate.path}>
                      {r.candidate.path}
                    </div>
                    <div className="mem-import-controls">
                      <select
                        value={r.scope}
                        onChange={(e) =>
                          setRows((prev) =>
                            prev.map((x, i) => (i === idx ? { ...x, scope: e.target.value as MemoryScope } : x)),
                          )
                        }
                      >
                        <option value="global">{t('globalScope')}</option>
                        <option value="project" disabled={!cwd && !projectKey}>
                          {t('projectScope')}
                        </option>
                      </select>
                      <select
                        value={r.mode}
                        onChange={(e) =>
                          setRows((prev) =>
                            prev.map((x, i) => (i === idx ? { ...x, mode: e.target.value as MemoryMode } : x)),
                          )
                        }
                      >
                        <option value="on-demand">{t('modeOnDemand')}</option>
                        <option value="always">{t('modeAlways')}</option>
                      </select>
                      <input
                        className="mem-import-id"
                        value={r.id}
                        onChange={(e) =>
                          setRows((prev) => prev.map((x, i) => (i === idx ? { ...x, id: e.target.value } : x)))
                        }
                        placeholder={t('fieldIdPlaceholder')}
                      />
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>
        <div className="mem-modal-foot">
          <button className="btn" onClick={onClose}>
            {t('cancel')}
          </button>
          <button
            className="btn primary"
            disabled={importing || selectedCount === 0}
            onClick={doImport}
          >
            {t('importSelected', { count: selectedCount })}
          </button>
        </div>
      </div>
    </div>
  );
}

function PendingList({
  hostId,
  pending,
  onEdit,
  onDone,
}: {
  hostId: string;
  pending: PendingMemory[];
  onEdit: (p: PendingMemory) => void;
  onDone: () => void;
}) {
  if (!pending.length) return null;

  const resolve = async (p: PendingMemory, approve: boolean) => {
    try {
      await rpc(hostId, 'memory.resolvePending', { pendingId: p.pendingId, approve });
      toast(approve ? t('approved') : t('rejected'));
      onDone();
    } catch (e) {
      errorToast(e);
    }
  };

  return (
    <div className="card mem-pending-card">
      <div className="card-title">{t('pendingHeading', { count: pending.length })}</div>
      <div className="mem-pending-list">
        {pending.map((p) => {
          const scopeLabel =
            p.scope === 'global'
              ? t('scopeGlobalShort')
              : p.projectName
                ? t('scopeProjectNamed', { name: p.projectName })
                : t('scopeProjectShort');
          return (
            <div key={p.pendingId} className="mem-pending-item">
              <div className="mem-pending-head">
                <strong>
                  {p.action === 'delete'
                    ? t('pendingDeleteTitle', { scope: scopeLabel, title: p.title || p.id })
                    : t('pendingSaveTitle', { scope: scopeLabel, title: p.title || p.id })}
                </strong>
                <code className="muted small">{p.id}</code>
              </div>
              {p.description && <div className="muted small">{p.description}</div>}
              {p.content && <pre className="mem-pending-snippet">{p.content.slice(0, 320)}</pre>}
              <div className="mem-item-actions">
                <button className="btn small primary" onClick={() => resolve(p, true)}>
                  {t('approve')}
                </button>
                {p.action !== 'delete' && (
                  <button className="btn small" onClick={() => onEdit(p)}>
                    {t('editAndApprove')}
                  </button>
                )}
                <button className="btn small" onClick={() => resolve(p, false)}>
                  {t('reject')}
                </button>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

export function MemoryView({ hostId, cwd }: { hostId: string; cwd?: string }) {
  useLang();
  const host = useStore((s) => s.hosts.find((h) => h.config.id === hostId));
  const home = hostStatus(hostId).hello?.home;
  const [data, setData] = useState<MemoryListResponse | null>(null);
  const [selected, setSelected] = useState<'global' | string>('global');
  const [query, setQuery] = useState('');
  const [editor, setEditor] = useState<{
    kind: 'create' | 'edit' | 'split' | 'review';
    initial: MemoryEditorInit;
  } | null>(null);
  const [previewOpen, setPreviewOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [initializedScope, setInitializedScope] = useState(false);

  const load = useCallback(() => {
    rpc<MemoryListResponse>(hostId, 'memory.list', { cwd })
      .then((res) => {
        setData(res);
        if (!initializedScope) {
          setInitializedScope(true);
          if (cwd && res.resolvedProject) {
            setSelected(res.resolvedProject.key);
          }
        }
      })
      .catch(errorToast);
  }, [hostId, cwd, initializedScope]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => onMemoryChanged((h) => h === hostId && load()), [hostId, load]);

  const activeProject = useMemo(() => {
    if (!data || selected === 'global') return null;
    if (data.resolvedProject?.key === selected) return data.resolvedProject;
    return data.projects.find((p) => p.key === selected) ?? null;
  }, [data, selected]);

  const activeList = useMemo(() => {
    if (!data) return [];
    const base =
      selected === 'global'
        ? data.global
        : data.byProject?.[selected] ?? (data.resolvedProject?.key === selected ? data.project : []);
    const q = query.trim().toLowerCase();
    if (!q) return base;
    return base.filter(
      (m) =>
        m.id.toLowerCase().includes(q) ||
        m.title.toLowerCase().includes(q) ||
        m.description.toLowerCase().includes(q) ||
        (m.tags ?? []).some((x) => x.toLowerCase().includes(q)),
    );
  }, [data, selected, query]);

  const openEdit = async (m: MemoryMeta, kind: 'edit' | 'split' = 'edit') => {
    try {
      const entry = await rpc<MemoryEntry>(hostId, 'memory.get', {
        scope: m.scope,
        projectKey: m.projectKey ?? undefined,
        id: m.id,
      });
      setEditor({
        kind,
        initial: {
          scope: kind === 'split' ? (m.scope === 'project' ? 'global' : 'project') : entry.scope,
          projectKey: entry.projectKey ?? activeProject?.key ?? data?.resolvedProject?.key,
          cwd: activeProject?.rootPath ?? cwd,
          id: kind === 'split' ? `${entry.id}-part` : entry.id,
          prevId: kind === 'edit' ? entry.id : undefined,
          title: entry.title,
          description: entry.description,
          mode: entry.mode,
          tags: entry.tags ?? [],
          content: entry.content,
        },
      });
    } catch (e) {
      errorToast(e);
    }
  };

  const toggleMode = async (m: MemoryMeta) => {
    try {
      const entry = await rpc<MemoryEntry>(hostId, 'memory.get', {
        scope: m.scope,
        projectKey: m.projectKey ?? undefined,
        id: m.id,
      });
      const nextMode: MemoryMode = entry.mode === 'always' ? 'on-demand' : 'always';
      await rpc(hostId, 'memory.save', {
        scope: entry.scope,
        projectKey: entry.projectKey ?? undefined,
        id: entry.id,
        title: entry.title,
        description: entry.description,
        mode: nextMode,
        tags: entry.tags ?? [],
        content: entry.content,
      });
      toast(t('saved'));
      load();
    } catch (e) {
      errorToast(e);
    }
  };

  const removeItem = async (m: MemoryMeta) => {
    const scopeText = m.scope === 'global' ? t('scopeGlobalShort') : t('scopeProjectShort');
    const ok = await askConfirm({
      title: t('deleteConfirm', { scope: scopeText, title: m.title }),
      okText: t('delete'),
      danger: true,
    });
    if (!ok) return;
    try {
      const res = await rpc<{ undoId?: string }>(hostId, 'memory.delete', {
        scope: m.scope,
        projectKey: m.projectKey ?? undefined,
        id: m.id,
      });
      toast(
        t('deleted', { title: m.title }),
        'info',
        res.undoId
          ? {
              label: t('undo'),
              run: () => {
                rpc(hostId, 'memory.undo', { undoId: res.undoId })
                  .then(() => {
                    toast(t('undone'));
                    load();
                  })
                  .catch(errorToast);
              },
            }
          : undefined,
      );
      load();
    } catch (e) {
      errorToast(e);
    }
  };

  const moveItem = async (m: MemoryMeta, toScope: MemoryScope, toProjectKey?: string) => {
    try {
      await rpc(hostId, 'memory.move', {
        fromScope: m.scope,
        fromProjectKey: m.projectKey ?? undefined,
        id: m.id,
        toScope,
        toProjectKey,
        cwd: toScope === 'project' && !toProjectKey ? cwd : undefined,
      });
      toast(t('moved'));
      load();
    } catch (e) {
      errorToast(e);
    }
  };

  const openItemMenu = (e: React.MouseEvent, m: MemoryMeta) => {
    const r = e.currentTarget.getBoundingClientRect();
    const projectTargets = data?.projects ?? [];
    openMenu({ clientX: r.right - 180, clientY: r.bottom + 4 }, [
      { label: t('edit'), icon: 'edit', run: () => openEdit(m, 'edit') },
      { label: t('split'), icon: 'columns', run: () => openEdit(m, 'split') },
      ...(m.scope === 'project'
        ? [{ label: t('moveToGlobal'), icon: 'globe', run: () => moveItem(m, 'global') }]
        : projectTargets.map((p) => ({
            label: `${t('moveToProject')} ${p.displayName}`,
            icon: 'folder',
            run: () => moveItem(m, 'project', p.key),
          }))),
      '-',
      { label: t('delete'), icon: 'trash', danger: true, run: () => removeItem(m) },
    ]);
  };

  if (!data) return <div className="page muted">{t('loading')}</div>;

  const projectList: MemoryProjectInfo[] = (() => {
    const map = new Map<string, MemoryProjectInfo>();
    if (data.resolvedProject) map.set(data.resolvedProject.key, data.resolvedProject);
    for (const p of data.projects) map.set(p.key, p);
    return [...map.values()];
  })();

  return (
    <div className="page mem-page">
      <div className="page-head">
        <h2>
          {host?.config.name} · {t('title')}
        </h2>
        <div className="row" style={{ gap: 8, flexWrap: 'wrap' }}>
          <button className="btn small" onClick={() => setPreviewOpen(true)}>
            <Icon name="eye" size={14} /> {t('previewL0')}
          </button>
          <button className="btn small" onClick={() => setImportOpen(true)}>
            <Icon name="download" size={14} /> {t('importFromTools')}
          </button>
          <button
            className="btn small primary"
            onClick={() =>
              setEditor({
                kind: 'create',
                initial: {
                  scope: selected === 'global' ? 'global' : 'project',
                  projectKey: activeProject?.key ?? data.resolvedProject?.key,
                  cwd: activeProject?.rootPath ?? cwd,
                  mode: 'on-demand',
                },
              })
            }
          >
            <Icon name="plus" size={14} /> {t('newMemory')}
          </button>
        </div>
      </div>

      {!data.settings.enabled && (
        <div className="card mem-disabled-banner">
          <span className="small">{t('disabledBanner')}</span>
          <button className="btn small" onClick={() => navigate({ kind: 'settings', hostId })}>
            <Icon name="settings" size={14} /> {t('openSettings')}
          </button>
        </div>
      )}

      <PendingList
        hostId={hostId}
        pending={data.pending}
        onEdit={(p) =>
          setEditor({
            kind: 'review',
            initial: {
              scope: p.scope,
              projectKey: p.projectKey,
              id: p.id,
              title: p.title,
              description: p.description,
              mode: p.mode,
              tags: p.tags,
              content: p.content,
              pendingId: p.pendingId,
            },
          })
        }
        onDone={load}
      />

      <div className="mem-layout">
        <div className="card mem-scopes">
          <button
            className={`mem-scope-row ${selected === 'global' ? 'active' : ''}`}
            onClick={() => setSelected('global')}
          >
            <Icon name="globe" size={15} />
            <span className="grow truncate">{t('globalScope')}</span>
            <span className="muted small">{data.global.length}</span>
          </button>
          <div className="mem-scope-sep">{t('projectsHeading')}</div>
          {projectList.length === 0 ? (
            <div className="muted small" style={{ padding: '6px 10px' }}>
              {t('noProjects')}
            </div>
          ) : (
            projectList.map((p) => (
              <button
                key={p.key}
                className={`mem-scope-row ${selected === p.key ? 'active' : ''}`}
                title={p.rootPath}
                onClick={() => setSelected(p.key)}
              >
                <Icon name="folder" size={15} />
                <div className="grow min0">
                  <div className="truncate">{p.displayName}</div>
                  {p.rootPath && <div className="muted small truncate">{shortPath(p.rootPath, home)}</div>}
                </div>
                <span className="muted small">{p.count}</span>
              </button>
            ))
          )}
          {cwd && data.projects.length > 0 && (!data.resolvedProject || data.resolvedProject.count === 0) && (
            <button
              className="btn small"
              style={{ marginTop: 8, width: '100%' }}
              onClick={(e) => {
                const r = e.currentTarget.getBoundingClientRect();
                openMenu(
                  { clientX: r.left, clientY: r.bottom + 4 },
                  data.projects.map((p) => ({
                    label: `${p.displayName} (${shortPath(p.rootPath, home)})`,
                    icon: 'link',
                    run: async () => {
                      try {
                        const updated = await rpc<MemoryProjectInfo>(hostId, 'memory.rebindProject', {
                          projectKey: p.key,
                          cwd,
                        });
                        toast(t('rebindSuccess', { name: updated.displayName }));
                        setSelected(updated.key);
                        load();
                      } catch (err) {
                        errorToast(err);
                      }
                    },
                  })),
                );
              }}
            >
              <Icon name="link" size={13} /> {t('rebindProject')}
            </button>
          )}
        </div>

        <div className="mem-main">
          <div className="mem-toolbar">
            <div className="mem-toolbar-head">
              <strong>{selected === 'global' ? t('globalScope') : activeProject?.displayName}</strong>
              <span className="muted small truncate">
                {selected === 'global'
                  ? t('globalDesc')
                  : activeProject?.rootPath
                    ? shortPath(activeProject.rootPath, home)
                    : ''}
              </span>
            </div>
            <input
              className="mem-search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={t('searchPlaceholder')}
            />
          </div>

          {activeList.length === 0 ? (
            <div className="card muted small">
              {selected === 'global' ? t('emptyGlobal') : t('emptyProject')}
            </div>
          ) : (
            <div className="mem-cards">
              {activeList.map((m) => (
                <div key={`${m.scope}:${m.projectKey ?? ''}:${m.id}`} className="card mem-card">
                  <div className="mem-card-head">
                    <div className="row grow min0" style={{ gap: 8, flexWrap: 'wrap' }}>
                      <strong className="truncate">{m.title}</strong>
                      <code className="muted small">{m.id}</code>
                      <button
                        type="button"
                        className={`acc-badge ${m.mode === 'always' ? 'acc-badge-accent' : 'acc-badge-muted'}`}
                        title={m.mode === 'always' ? t('modeAlwaysHint') : t('modeOnDemandHint')}
                        onClick={() => toggleMode(m)}
                      >
                        {m.mode === 'always' ? t('modeAlways') : t('modeOnDemand')}
                      </button>
                      <span className="muted small">{fmtBytes(m.size)}</span>
                    </div>
                    <div className="row" style={{ gap: 4 }}>
                      <button className="icon-btn" title={t('edit')} onClick={() => openEdit(m, 'edit')}>
                        <Icon name="edit" size={14} />
                      </button>
                      <button className="icon-btn" title={t('edit')} onClick={(e) => openItemMenu(e, m)}>
                        <Icon name="more" size={15} />
                      </button>
                    </div>
                  </div>
                  {m.description && <div className="small mem-card-desc">{m.description}</div>}
                  {!!m.tags?.length && (
                    <div className="mem-tags">
                      {m.tags.map((tag) => (
                        <span key={tag} className="mem-tag">
                          {tag}
                        </span>
                      ))}
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>
      </div>

      {editor && (
        <MemoryEditorModal
          hostId={hostId}
          kind={editor.kind}
          initial={editor.initial}
          projects={data.projects}
          resolvedProject={data.resolvedProject}
          cwd={activeProject?.rootPath ?? cwd}
          onSaved={load}
          onClose={() => setEditor(null)}
        />
      )}
      {previewOpen && (
        <MemoryPreviewModal
          hostId={hostId}
          cwd={activeProject?.rootPath ?? cwd}
          projectKey={activeProject?.key}
          onClose={() => setPreviewOpen(false)}
        />
      )}
      {importOpen && (
        <MemoryImportModal
          hostId={hostId}
          cwd={activeProject?.rootPath ?? cwd}
          projectKey={activeProject?.key ?? data.resolvedProject?.key}
          onImported={load}
          onClose={() => setImportOpen(false)}
        />
      )}
    </div>
  );
}

/** Session-scoped quick Memory drawer opened from inside a chat session. */
export function SessionMemoryDrawer({
  hostId,
  cwd,
  onClose,
}: {
  hostId: string;
  cwd: string;
  onClose: () => void;
}) {
  useLang();
  useBack(true, onClose);
  const [data, setData] = useState<MemoryListResponse | null>(null);
  const [editor, setEditor] = useState<{
    kind: 'create' | 'edit' | 'review';
    initial: MemoryEditorInit;
  } | null>(null);
  const [previewOpen, setPreviewOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);

  const load = useCallback(() => {
    rpc<MemoryListResponse>(hostId, 'memory.list', { cwd })
      .then(setData)
      .catch(errorToast);
  }, [hostId, cwd]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => onMemoryChanged((h) => h === hostId && load()), [hostId, load]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !editor && !previewOpen && !importOpen) onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose, editor, previewOpen, importOpen]);

  const openEdit = async (m: MemoryMeta) => {
    try {
      const entry = await rpc<MemoryEntry>(hostId, 'memory.get', {
        scope: m.scope,
        projectKey: m.projectKey ?? undefined,
        id: m.id,
      });
      setEditor({
        kind: 'edit',
        initial: {
          scope: entry.scope,
          projectKey: entry.projectKey,
          cwd,
          id: entry.id,
          prevId: entry.id,
          title: entry.title,
          description: entry.description,
          mode: entry.mode,
          tags: entry.tags ?? [],
          content: entry.content,
        },
      });
    } catch (e) {
      errorToast(e);
    }
  };

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal mem-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>{t('sessionDrawerTitle')}</h3>
          <button className="icon-btn" onClick={onClose} aria-label={t('close')}>
            <Icon name="x" size={16} />
          </button>
        </div>
        <div className="mem-modal-body">
          <p className="muted small" style={{ marginBottom: 10 }}>
            {t('sessionDrawerDesc')}
          </p>
          {!data ? (
            <div className="muted small">{t('loading')}</div>
          ) : (
            <>
              {!data.settings.enabled && (
                <div className="card mem-disabled-banner" style={{ marginBottom: 10 }}>
                  <span className="small">{t('disabledBanner')}</span>
                </div>
              )}
              <PendingList
                hostId={hostId}
                pending={data.pending}
                onEdit={(p) =>
                  setEditor({
                    kind: 'review',
                    initial: {
                      scope: p.scope,
                      projectKey: p.projectKey,
                      id: p.id,
                      title: p.title,
                      description: p.description,
                      mode: p.mode,
                      tags: p.tags,
                      content: p.content,
                      pendingId: p.pendingId,
                    },
                  })
                }
                onDone={load}
              />
              <div className="mem-drawer-sec">
                <div className="mem-drawer-title">
                  <strong>{t('globalScope')}</strong>
                  <span className="muted small">{data.global.length}</span>
                </div>
                {data.global.length === 0 ? (
                  <div className="muted small">{t('emptyGlobal')}</div>
                ) : (
                  data.global.map((m) => (
                    <button key={m.id} className="mem-drawer-row" onClick={() => openEdit(m)}>
                      <span className="grow truncate">
                        <strong>{m.title}</strong> <code className="muted small">{m.id}</code>
                      </span>
                      <span className={`acc-badge ${m.mode === 'always' ? 'acc-badge-accent' : 'acc-badge-muted'}`}>
                        {m.mode === 'always' ? t('modeAlways') : t('modeOnDemand')}
                      </span>
                    </button>
                  ))
                )}
              </div>
              {data.resolvedProject && (
                <div className="mem-drawer-sec">
                  <div className="mem-drawer-title">
                    <strong>{t('currentDirProject', { name: data.resolvedProject.displayName })}</strong>
                    <span className="muted small">{data.project.length}</span>
                  </div>
                  {data.project.length === 0 ? (
                    <div className="muted small">{t('emptyProject')}</div>
                  ) : (
                    data.project.map((m) => (
                      <button key={m.id} className="mem-drawer-row" onClick={() => openEdit(m)}>
                        <span className="grow truncate">
                          <strong>{m.title}</strong> <code className="muted small">{m.id}</code>
                        </span>
                        <span className={`acc-badge ${m.mode === 'always' ? 'acc-badge-accent' : 'acc-badge-muted'}`}>
                          {m.mode === 'always' ? t('modeAlways') : t('modeOnDemand')}
                        </span>
                      </button>
                    ))
                  )}
                </div>
              )}
            </>
          )}
        </div>
        <div className="mem-modal-foot">
          <button
            className="btn small"
            onClick={() => {
              onClose();
              navigate({ kind: 'memory', hostId, cwd });
            }}
          >
            {t('manageAll')}
          </button>
          <button className="btn small" onClick={() => setPreviewOpen(true)}>
            <Icon name="eye" size={14} /> {t('previewL0')}
          </button>
          <button className="btn small" onClick={() => setImportOpen(true)}>
            <Icon name="download" size={14} /> {t('importFromTools')}
          </button>
          <button
            className="btn small primary"
            onClick={() =>
              setEditor({
                kind: 'create',
                initial: {
                  scope: data?.resolvedProject ? 'project' : 'global',
                  projectKey: data?.resolvedProject?.key,
                  cwd,
                  mode: 'on-demand',
                },
              })
            }
          >
            <Icon name="plus" size={14} /> {t('newMemory')}
          </button>
        </div>
      </div>

      {editor && data && (
        <MemoryEditorModal
          hostId={hostId}
          kind={editor.kind}
          initial={editor.initial}
          projects={data.projects}
          resolvedProject={data.resolvedProject}
          cwd={cwd}
          onSaved={load}
          onClose={() => setEditor(null)}
        />
      )}
      {previewOpen && (
        <MemoryPreviewModal
          hostId={hostId}
          cwd={cwd}
          projectKey={data?.resolvedProject?.key}
          onClose={() => setPreviewOpen(false)}
        />
      )}
      {importOpen && (
        <MemoryImportModal
          hostId={hostId}
          cwd={cwd}
          projectKey={data?.resolvedProject?.key}
          onImported={load}
          onClose={() => setImportOpen(false)}
        />
      )}
    </div>
  );
}
