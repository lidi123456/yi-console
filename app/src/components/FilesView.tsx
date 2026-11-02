import { useEffect, useState } from 'react';
import { useBack } from '../back';
import { tr } from '../i18n';
import { errorToast, navigate, setState, useStore } from '../store';
import { rpc } from '../transport';
import type { DirEntry } from '../types';
import { fmtSize, timeAgo } from '../util';
import { Icon } from './Icon';

interface Listing {
  path: string;
  parent: string | null;
  entries: DirEntry[];
}

/** Windows 主机的盘符列表（C:\\、D:\\…），其他系统返回空 */
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
  const [hidden, setHidden] = useState(true);
  const drives = useDrives(hostId);
  const [file, setFile] = useState<{ path: string; content: string; binary: boolean; truncated: boolean; size: number } | null>(null);
  useBack(!!file, () => setFile(null));

  useEffect(() => setCur(path || '~'), [path]);
  useEffect(() => {
    rpc<Listing>(hostId, 'fs.list', { path: cur, showHidden: hidden })
      .then(setList)
      .catch(errorToast);
  }, [hostId, cur, hidden]);

  const crumbs = (list?.path ?? '').split(/(?<=[\\/])/).filter(Boolean);
  const open = (e: DirEntry) => {
    if (e.isDir) {
      setFile(null);
      setCur(e.path);
    } else {
      rpc(hostId, 'fs.read', { path: e.path }).then(setFile).catch(errorToast);
    }
  };

  return (
    <div className={`page split ${file ? 'has-sel' : ''}`}>
      <div className="split-list">
        <div className="page-head">
          <h2>{tr('文件', 'Files')}</h2>
          <button
            className="btn small"
            disabled={!list}
            onClick={() => list && setState({ newSession: { hostId, cwd: list.path } })}
          >
            <Icon name="plus" size={14} /> {tr('在此新建会话', 'New session here')}
          </button>
        </div>
        {drives.length > 1 && (
          <div className="seg drives">
            {drives.map((d) => (
              <button
                key={d}
                className={list?.path.toUpperCase().startsWith(d.toUpperCase()) ? 'active' : ''}
                onClick={() => {
                  setFile(null);
                  setCur(d);
                }}
              >
                {d.replace(/\\$/, '')}
              </button>
            ))}
          </div>
        )}
        <div className="crumbs mono small">
          {crumbs.map((c, i) => (
            <button key={i} onClick={() => setCur(crumbs.slice(0, i + 1).join(''))}>
              {c}
            </button>
          ))}
        </div>
        <label className="check small">
          <input type="checkbox" checked={hidden} onChange={(e) => setHidden(e.target.checked)} /> {tr('显示隐藏文件', 'Show hidden files')}
        </label>
        <div className="list">
          {list?.parent && (
            <button className="list-row" onClick={() => setCur(list.parent!)}>
              <Icon name="up" size={16} /> ..
            </button>
          )}
          {list?.entries.map((e) => (
            <button key={e.path} className={`list-row ${file?.path === e.path ? 'active' : ''}`} onClick={() => open(e)}>
              <Icon name={e.isDir ? 'folder' : 'file'} size={16} />
              <span className="grow truncate">{e.name}</span>
              <span className="muted small">{e.isDir ? '' : fmtSize(e.size)}</span>
              <span className="muted small hide-mobile">{timeAgo(e.modified)}</span>
            </button>
          ))}
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
              <span className="muted small">{fmtSize(file.size)}</span>
            </div>
            {file.binary ? (
              <div className="center-msg muted">{tr('二进制文件，无法预览', 'Binary file, cannot preview')}</div>
            ) : (
              <pre className="file-view mono">
                {file.content.split('\n').map((l, i) => (
                  <div key={i} className="fl">
                    <span className="ln">{i + 1}</span>
                    <span>{l || ' '}</span>
                  </div>
                ))}
                {file.truncated && <div className="muted">{tr('… 文件过大，只显示前 1MB', '… File is large; showing the first 1MB')}</div>}
              </pre>
            )}
          </>
        ) : (
          <div className="center-msg muted">
            {tr('选择文件预览', 'Select a file to preview')}
            <button className="link-btn" onClick={() => navigate({ kind: 'history', hostId })}>
              {tr('查看历史会话', 'View history')}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
