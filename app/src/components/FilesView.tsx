import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT } from '../i18n';
import { errorToast, navigate, openNewSession, useStore } from '../store';
import { rpc } from '../transport';
import type { DirEntry } from '../types';
import { fmtSize, timeAgo } from '../util';
import { CodeEditor } from './CodeEditor';
import { Icon } from './Icon';

const t = getT('filesView');

interface Listing {
  path: string;
  parent: string | null;
  entries: DirEntry[];
}

interface FileData {
  path: string;
  content: string;
  binary: boolean;
  truncated: boolean;
  size: number;
  modified?: number;
}

/** Drive list on Windows hosts (C:\\, D:\\…); empty on other systems */
export function useDrives(hostId: string) {
  const isWin = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.status.hello?.os === 'windows');
  const [drives, setDrives] = useState<string[]>([]);
  useEffect(() => {
    if (!isWin) return setDrives([]);
    rpc<string[]>(hostId, 'fs.roots')
      .then(setDrives)
      .catch(() => setDrives([]));
  }, [hostId, isWin]);
  return drives;
}

export function FilesView({ hostId, path }: { hostId: string; path?: string }) {
  const [cur, setCur] = useState(path || '~');
  const [list, setList] = useState<Listing | null>(null);
  const [listLoading, setListLoading] = useState(true);
  const [hidden, setHidden] = useState(true);
  const drives = useDrives(hostId);
  const [file, setFile] = useState<FileData | null>(null);
  const [fileLoading, setFileLoading] = useState(false);
  const [wrap, setWrap] = useState(false);
  const listReqRef = useRef(0);
  const fileReqRef = useRef(0);
  const splitListRef = useRef<HTMLDivElement>(null);
  const listRowsRef = useRef<HTMLDivElement>(null);
  const [rowH, setRowH] = useState(38);
  const rowHRef = useRef(rowH);
  rowHRef.current = rowH;
  const [win, setWin] = useState<[number, number]>([0, 40]);
  useBack(!!file, () => setFile(null));

  useEffect(() => setCur(path || '~'), [path]);
  useEffect(() => {
    const req = ++listReqRef.current;
    setListLoading(true);
    rpc<Listing>(hostId, 'fs.list', { path: cur, showHidden: hidden })
      .then((r) => {
        if (listReqRef.current === req) setList(r);
      })
      .catch((e) => {
        if (listReqRef.current === req) errorToast(e);
      })
      .finally(() => {
        if (listReqRef.current === req) setListLoading(false);
      });
  }, [hostId, cur, hidden]);

  const entries = list?.entries ?? [];
  const virtual = entries.length > 100;

  useLayoutEffect(() => {
    if (splitListRef.current) splitListRef.current.scrollTop = 0;
    setWin([0, 40]);
  }, [list?.path]);

  useLayoutEffect(() => {
    const scroller = splitListRef.current;
    const rowsBox = listRowsRef.current;
    if (!virtual || !scroller || !rowsBox) return;
    let raf = 0;
    const calc = () => {
      const sample = rowsBox.querySelector<HTMLElement>('.list-row');
      const h = sample?.offsetHeight || rowHRef.current;
      if (h !== rowHRef.current) setRowH(h);
      const boxTop = rowsBox.getBoundingClientRect().top - scroller.getBoundingClientRect().top + scroller.scrollTop;
      const viewTop = Math.max(0, scroller.scrollTop - boxTop);
      const viewBot = Math.max(0, scroller.scrollTop + scroller.clientHeight - boxTop);
      const from = Math.max(0, Math.floor(viewTop / h) - 15);
      const to = Math.min(entries.length, Math.ceil(viewBot / h) + 15);
      setWin((p) => (p[0] === from && p[1] === to ? p : [from, to]));
    };
    const onScroll = () => {
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        calc();
      });
    };
    calc();
    scroller.addEventListener('scroll', onScroll, { passive: true });
    const ro = new ResizeObserver(onScroll);
    ro.observe(scroller);
    return () => {
      scroller.removeEventListener('scroll', onScroll);
      ro.disconnect();
      if (raf) cancelAnimationFrame(raf);
    };
  }, [virtual, entries.length]);

  // Keep separators without lookbehind, which older macOS WebKit cannot parse.
  const crumbs = (list?.path ?? '').match(/[^\\/]*[\\/]|[^\\/]+$/g) ?? [];
  const navigateDir = (nextDir: string, clearFile = false) => {
    if (clearFile) {
      ++fileReqRef.current;
      setFileLoading(false);
      setFile(null);
    }
    setCur(nextDir);
  };
  const open = (e: DirEntry) => {
    if (e.isDir) {
      navigateDir(e.path, true);
    } else {
      const req = ++fileReqRef.current;
      setFileLoading(true);
      rpc<FileData>(hostId, 'fs.read', { path: e.path })
        .then((r) => {
          if (fileReqRef.current === req) setFile(r);
        })
        .catch((err) => {
          if (fileReqRef.current === req) errorToast(err);
        })
        .finally(() => {
          if (fileReqRef.current === req) setFileLoading(false);
        });
    }
  };

  const from = virtual ? Math.min(win[0], entries.length) : 0;
  const to = virtual ? Math.min(Math.max(win[1], from), entries.length) : entries.length;
  const filename = file ? file.path.split(/[\\/]/).pop() ?? file.path : '';

  return (
    <div className={`page split ${file || fileLoading ? 'has-sel' : ''}`}>
      <div className="split-list" ref={splitListRef}>
        <div className="page-head">
          <div className="row">
            <h2>{t('files')}</h2>
            {listLoading && <span className="spinner mini" aria-label={t('loadingDirectory')} />}
          </div>
          <button
            className="btn small"
            disabled={!list}
            onClick={() => list && openNewSession({ hostId, cwd: list.path })}
          >
            <Icon name="plus" size={14} /> {t('newSessionHere')}
          </button>
        </div>
        {drives.length > 1 && (
          <div className="seg drives">
            {drives.map((d) => (
              <button
                key={d}
                className={list?.path.toUpperCase().startsWith(d.toUpperCase()) ? 'active' : ''}
                onClick={() => navigateDir(d, true)}
              >
                {d.replace(/\\$/, '')}
              </button>
            ))}
          </div>
        )}
        <div className="crumbs mono small">
          {crumbs.map((c, i) => (
            <button key={i} onClick={() => navigateDir(crumbs.slice(0, i + 1).join(''))}>
              {c}
            </button>
          ))}
        </div>
        <label className="check small">
          <input type="checkbox" checked={hidden} onChange={(e) => setHidden(e.target.checked)} /> {t('showHiddenFiles')}
        </label>
        <div className="list">
          {list?.parent && (
            <button className="list-row" onClick={() => navigateDir(list.parent!)}>
              <Icon name="up" size={16} /> ..
            </button>
          )}
          {listLoading && !list ? (
            <div className="files-loading muted small" role="status">
              <span className="spinner mini" />
              <span>{t('loadingDirectory')}</span>
            </div>
          ) : (
            <div
              ref={listRowsRef}
              style={virtual ? { paddingTop: from * rowH, paddingBottom: (entries.length - to) * rowH } : undefined}
            >
              {entries.slice(from, to).map((e) => (
                <button key={e.path} className={`list-row ${file?.path === e.path ? 'active' : ''}`} onClick={() => open(e)}>
                  <Icon name={e.isDir ? 'folder' : 'file'} size={16} />
                  <span className="grow truncate">{e.name}</span>
                  <span className="muted small">{e.isDir ? '' : fmtSize(e.size)}</span>
                  <span className="muted small hide-mobile">{timeAgo(e.modified)}</span>
                </button>
              ))}
            </div>
          )}
        </div>
      </div>
      <div className="split-detail">
        {file ? (
          <>
            <div className="detail-head">
              <button className="icon-btn show-mobile" onClick={() => setFile(null)}>
                <Icon name="x" />
              </button>
              <span className="mono small truncate grow">{file.path}</span>
              {!file.binary && (
                <button className={`icon-btn ${wrap ? 'active' : ''}`} title={t('toggleWordWrap')} onClick={() => setWrap((w) => !w)}>
                  <Icon name="wrap" size={14} />
                </button>
              )}
              {fileLoading && <span className="spinner mini" aria-label={t('loadingFile')} />}
              <span className="muted small">{fmtSize(file.size)}</span>
            </div>
            {file.truncated && <div className="files-truncated muted small">{t('fileIsLargeShowingThe')}</div>}
            {file.binary ? (
              <div className="center-msg muted">{t('binaryFileCannotPreview')}</div>
            ) : (
              <div className="files-code-wrap">
                <CodeEditor
                  docKey={`${file.path}#${file.modified ?? 0}`}
                  value={file.content}
                  filename={filename}
                  readOnly
                  wrap={wrap}
                  onChange={() => {}}
                  onSave={() => {}}
                  onToggleWrap={() => setWrap((w) => !w)}
                />
              </div>
            )}
            {fileLoading && (
              <div className="wp-loading-overlay" role="status">
                <span className="spinner" />
                <span>{t('loadingFile')}</span>
              </div>
            )}
          </>
        ) : fileLoading ? (
          <div className="center-msg muted files-loading" role="status">
            <span className="spinner" />
            <span>{t('loadingFile')}</span>
          </div>
        ) : (
          <div className="center-msg muted">
            {t('selectAFileToPreview')}
            <button className="link-btn" onClick={() => navigate({ kind: 'history', hostId })}>
              {t('viewHistory')}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
