// Browser regression fixture for floating CodeMirror Find & Replace panel and modal z-index isolation.
import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { CodeEditor } from '../src/components/CodeEditor';
import { setLangPref } from '../src/i18n';
import { setTheme } from '../src/theme';
import '../src/styles.css';

const SAMPLE_CONF = [
  'nginx_http_gzip_types = text/plain application/json',
  'nginx_http_gzip_proxied = any',
  'nginx_http_gzip_vary = on',
  'nginx_http_keepalive_requests = 10000',
  'upstream_keepalive_pool_size = 256',
  'nginx_http_keepalive_timeout = 65s',
  '# Default keepalive pool per upstream target',
  'upstream_keepalive_pool_size_max = 512',
  'UPSTREAM_KEEPALIVE_POOL_SIZE = 1024',
  'nginx_main_worker_processes = auto',
  'nginx_events_worker_connections = 10000',
  '# Limit open file descriptors for high concurrency',
  'nginx_main_worker_rlimit_nofile = 65535',
  'upstream_keepalive_pool_size = 128',
].join('\n');

const params = new URLSearchParams(location.search);
setLangPref(params.get('lang') === 'en' ? 'en' : 'zh');
setTheme(params.get('theme') === 'dark' ? 'dark' : 'light');

function Fixture() {
  const [text, setText] = useState(SAMPLE_CONF);
  const [readOnly, setReadOnly] = useState(params.get('ro') === '1');
  const [modalOpen, setModalOpen] = useState(false);

  return (
    <div className="app" style={{ height: '100vh', display: 'flex', flexDirection: 'column', padding: 12 }}>
      <div style={{ display: 'flex', gap: 8, marginBottom: 8 }}>
        <button id="toggle-ro" className="btn xs" onClick={() => setReadOnly((v) => !v)}>
          {readOnly ? 'RO' : 'RW'}
        </button>
        <button id="toggle-modal" className="btn xs" onClick={() => setModalOpen((v) => !v)}>
          Modal
        </button>
        <span id="doc-len">{text.length}</span>
      </div>
      <div className="main" style={{ flex: 1, minHeight: 0, display: 'flex', margin: 0 }}>
        <div className="file-panel" style={{ width: '100%', maxWidth: 'none', flex: 1 }}>
          <div className="wp-body">
            <div className="wp-main">
              <CodeEditor
                docKey="kong.conf.prod"
                value={text}
                filename="kong.conf.prod"
                readOnly={readOnly}
                wrap={false}
                onChange={setText}
                onSave={() => {}}
                onAddSelection={() => {}}
              />
            </div>
          </div>
        </div>
      </div>
      {modalOpen && (
        <div className="modal-bg" id="test-modal-bg" onClick={() => setModalOpen(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <h3>用量</h3>
            </div>
            <div className="muted small">验证弹窗遮罩可以完整覆盖编辑器浮层搜索框。</div>
          </div>
        </div>
      )}
    </div>
  );
}

createRoot(document.getElementById('root')!).render(<Fixture />);
