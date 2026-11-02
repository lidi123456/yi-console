import { useState } from 'react';
import { HANDOFF_VARS, customHandoffTemplate, defaultHandoffTemplate, setHandoffTemplate } from '../handoff';
import { tr, useLang } from '../i18n';
import { toast } from '../store';
import { Icon } from './Icon';

/** "Continue with another tool" in system settings: edit the template for the first message */
export function HandoffCard() {
  useLang();
  const [custom, setCustom] = useState(() => customHandoffTemplate());
  // Show the default template (which follows the UI language) until it's edited
  const [text, setText] = useState<string | null>(null);
  const value = text ?? custom ?? defaultHandoffTemplate();
  const dirty = text !== null && text !== (custom ?? defaultHandoffTemplate());

  const save = () => {
    setHandoffTemplate(value);
    setCustom(customHandoffTemplate());
    setText(null);
    toast(tr('已保存', 'Saved'));
  };
  const reset = () => {
    setHandoffTemplate(null);
    setCustom(null);
    setText(null);
    toast(tr('已恢复默认模板', 'Restored default template'));
  };

  return (
    <div className="card">
      <div className="card-title">
        <Icon name="forward" size={16} /> {tr('换个工具继续', 'Continue in Another Tool')}
      </div>
      <p className="muted small">
        {tr(
          '会话菜单里「换个工具继续」时，新会话第一条消息用这个模板生成，打开对话框后还可以再改。',
          'When you use "Continue in Another Tool" from the session menu, the new session\'s first message is generated from this template. You can still edit it in the dialog.',
        )}
      </p>
      <textarea className="mono" rows={14} value={value} onChange={(e) => setText(e.target.value)} spellCheck={false} />
      <div className="muted small" style={{ marginTop: 8 }}>
        {tr('可用的占位符（生成时替换成实际内容）：', 'Available placeholders (replaced with actual values):')}
        <ul style={{ margin: '4px 0 0', paddingLeft: 18 }}>
          {HANDOFF_VARS.map(([k, d]) => (
            <li key={k}>
              <code>{k}</code> — {d()}
            </li>
          ))}
        </ul>
        {tr(
          '留空或和默认一样时使用默认模板（跟随界面语言）。只保存在这台设备上。',
          'Leave empty or identical to the default to use the default template (follows the UI language). Saved on this device only.',
        )}
      </div>
      <div className="row end" style={{ marginTop: 10 }}>
        <button className="btn" disabled={!custom && !dirty} onClick={reset}>
          {tr('恢复默认', 'Restore Default')}
        </button>
        <button className="btn primary" disabled={!dirty} onClick={save}>
          {tr('保存', 'Save')}
        </button>
      </div>
    </div>
  );
}
