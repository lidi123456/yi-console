// Browser fixture for the real chat renderer. Run with Vite, then tests/diagrams.mjs on Windows.
import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { Overlays } from '../src/components/Overlay';
import { ChatMarkdown } from '../src/components/ChatMarkdown';
import { setTheme } from '../src/theme';
import { setLangPref } from '../src/i18n';
import { renderDiagramMarkdown } from '../src/util';
import { diagramPng, renderDiagram, validateDiagram } from '../src/mermaid';
import '../src/styles.css';

function Fixture() {
  const [message, setMessage] = useState({ text: '', streaming: true });
  Object.assign(window, { setMessage, setTheme, renderDiagramMarkdown, renderDiagram, diagramPng, validateDiagram });
  return (
    <main style={{ width: 'min(850px, 100%)', padding: 16 }}>
      <ChatMarkdown {...message} />
      <Overlays />
    </main>
  );
}
setLangPref('en');
createRoot(document.getElementById('root')!).render(<Fixture />);
