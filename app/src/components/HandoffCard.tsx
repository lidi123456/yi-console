import { useState } from 'react';
import { HANDOFF_VARS, customHandoffTemplate, defaultHandoffTemplate, setHandoffTemplate } from '../handoff';
import { getT, useLang } from '../i18n';
import { toast } from '../store';
import { Icon } from './Icon';

const t = getT('handoffCard');

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
    toast(t('saved'));
  };
  const reset = () => {
    setHandoffTemplate(null);
    setCustom(null);
    setText(null);
    toast(t('restoredDefaultTemplate'));
  };

  return (
    <div className="card">
      <div className="card-title">
        <Icon name="forward" size={16} /> {t('continueInAnotherTool')}
      </div>
      <p className="muted small">
        {t('whenYouUseContinueIn')}
      </p>
      <textarea className="mono" rows={14} value={value} onChange={(e) => setText(e.target.value)} spellCheck={false} />
      <div className="muted small" style={{ marginTop: 8 }}>
        {t('availablePlaceholdersReplacedWithActual')}
        <ul style={{ margin: '4px 0 0', paddingLeft: 18 }}>
          {HANDOFF_VARS.map(([k, d]) => (
            <li key={k}>
              <code>{k}</code> — {d()}
            </li>
          ))}
        </ul>
        {t('leaveEmptyOrIdenticalTo')}
      </div>
      <div className="row end" style={{ marginTop: 10 }}>
        <button className="btn" disabled={!custom && !dirty} onClick={reset}>
          {t('restoreDefault')}
        </button>
        <button className="btn primary" disabled={!dirty} onClick={save}>
          {t('save')}
        </button>
      </div>
    </div>
  );
}
