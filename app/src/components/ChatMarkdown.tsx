import { memo, useMemo, useRef, useLayoutEffect } from 'react';
import { createPortal } from 'react-dom';
import { hydrateImages, type ImgCtx } from '../chatImages';
import { renderDiagramMarkdown } from '../util';
import { MermaidBlock } from './MermaidBlock';

export const ChatMarkdown = memo(function ChatMarkdown({
  text,
  streaming = false,
  ctx = null,
}: {
  text: string;
  streaming?: boolean;
  ctx?: ImgCtx | null;
}) {
  const model = useMemo(() => renderDiagramMarkdown(text), [text]);
  const ref = useRef<HTMLDivElement>(null);
  const previous = useRef<HTMLElement[]>([]);
  const targets = useMemo(() => model.diagrams.map((_, index) => previous.current[index] ?? document.createElement('div')), [model]);
  useLayoutEffect(() => {
    const root = ref.current;
    if (!root) return;
    const template = document.createElement('template');
    template.innerHTML = model.html;
    for (const placeholder of template.content.querySelectorAll<HTMLElement>(`[data-diagram="${model.marker}"]`)) {
      const target = targets[Number(placeholder.dataset.index)];
      if (target) placeholder.replaceWith(target);
    }
    // Move existing portal targets into the new tree: completed diagrams keep their React state while text streams.
    root.replaceChildren(template.content);
    previous.current = targets;
    hydrateImages(root, ctx);
  }, [model, targets, ctx]);
  return (
    <>
      <div ref={ref} className="md" />
      {model.diagrams.map((diagram, index) =>
        createPortal(<MermaidBlock {...diagram} streaming={streaming} />, targets[index], String(index)),
      )}
    </>
  );
});
