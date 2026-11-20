import { EditorSelection, StateEffect, type Extension } from '@codemirror/state';
import { EditorView, runScopeHandlers, type Command, type KeyBinding, type Panel, type ViewUpdate } from '@codemirror/view';
import {
  closeSearchPanel,
  findNext,
  findPrevious,
  getSearchQuery,
  openSearchPanel,
  replaceAll,
  replaceNext,
  search,
  searchKeymap,
  SearchQuery,
  setSearchQuery,
} from '@codemirror/search';
import { getT, isEn } from '../i18n';

const tx = getT('filePanel');

/** Cap match counting to keep 1MB files responsive during live typing */
const MAX_MATCH_COUNT = 2000;

/** State effect to expand/collapse the replace row inside the custom search panel */
const setReplaceOpen = StateEffect.define<boolean>();

const IS_MAC = typeof navigator !== 'undefined' && navigator.platform.includes('Mac');

function svgIcon(pathD: string, size = 14): SVGSVGElement {
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns, 'svg');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.9');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS(ns, 'path');
  path.setAttribute('d', pathD);
  svg.appendChild(path);
  return svg;
}

const PATH_CHEVRON = 'M9 6l6 6-6 6';
const PATH_UP = 'M18 15l-6-6-6 6';
const PATH_DOWN = 'M6 9l6 6 6-6';
const PATH_CLOSE = 'M6 6l12 12M18 6L6 18';

function scrollMatchIntoView(range: { from: number; to: number }) {
  return EditorView.scrollIntoView(EditorSelection.range(range.from, range.to), { y: 'nearest', yMargin: 56 });
}

interface MatchSpan {
  from: number;
  to: number;
}

function collectMatches(view: EditorView, query: SearchQuery): { items: MatchSpan[]; capped: boolean } {
  if (!query.search || !query.valid) return { items: [], capped: false };
  const items: MatchSpan[] = [];
  let capped = false;
  try {
    const cursor = query.getCursor(view.state);
    let step = cursor.next();
    while (!step.done) {
      if (items.length >= MAX_MATCH_COUNT) {
        capped = true;
        break;
      }
      items.push({ from: step.value.from, to: step.value.to });
      step = cursor.next();
    }
  } catch {
    return { items: [], capped: false };
  }
  return { items, capped };
}

function findMatchIndex(items: MatchSpan[], from: number, to: number): number {
  let lo = 0;
  let hi = items.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const m = items[mid];
    if (m.from === from && m.to === to) return mid;
    if (m.from < from || (m.from === from && m.to < to)) lo = mid + 1;
    else hi = mid - 1;
  }
  return -1;
}

function findFirstMatchAtOrAfter(items: MatchSpan[], pos: number): MatchSpan | null {
  if (!items.length) return null;
  let lo = 0;
  let hi = items.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (items[mid].from < pos) lo = mid + 1;
    else hi = mid;
  }
  return items[lo] ?? items[0];
}

/** Open the search panel with the replace row expanded (read-only editors open find-only) */
export const openReplacePanel: Command = (view) => {
  openSearchPanel(view);
  if (!view.state.readOnly) {
    view.dispatch({ effects: setReplaceOpen.of(true) });
  }
  return true;
};

/**
 * Floating top-right Find & Replace panel for CodeMirror 6 (modeled on T3 Code / VS Code).
 * - Click-through host overlay so code to the left stays interactive without shifting line 1.
 * - Single-row compact default with inline Aa / ab / .* toggles and live N / M match counter.
 * - Collapsible replace row hidden automatically in read-only editors and diff views.
 */
function createEditorSearchPanel(view: EditorView): Panel {
  let query = getSearchQuery(view.state);
  let replaceOpen = false;
  let caseSensitive = query.caseSensitive;
  let wholeWord = query.wholeWord;
  let regexp = query.regexp;
  let lastLangEn = isEn();
  let searchAnchor = view.state.selection.main.from;
  let cachedDoc = view.state.doc;
  let cachedQuery = query;
  let cachedMatches = collectMatches(view, query);

  const host = document.createElement('div');
  host.className = 'cm-search-host';

  const box = document.createElement('div');
  box.className = 'ed-find';
  box.setAttribute('role', 'search');
  host.appendChild(box);

  // Replace disclosure toggle (spans both rows on the left)
  const toggleBtn = document.createElement('button');
  toggleBtn.type = 'button';
  toggleBtn.className = 'ed-find-toggle';
  toggleBtn.setAttribute('aria-expanded', 'false');
  const toggleIconWrap = document.createElement('span');
  toggleIconWrap.className = 'ed-find-chev';
  toggleIconWrap.appendChild(svgIcon(PATH_CHEVRON, 13));
  toggleBtn.appendChild(toggleIconWrap);
  box.appendChild(toggleBtn);

  // Grid holding row 1 (find + count + nav) and row 2 (replace + actions) so both inputs share exact width
  const grid = document.createElement('div');
  grid.className = 'ed-find-grid';
  box.appendChild(grid);

  // Row 1, Col 1: Find input + inline Aa / ab / .* toggles
  const findField = document.createElement('div');
  findField.className = 'ed-find-field';
  grid.appendChild(findField);

  const searchInput = document.createElement('input');
  searchInput.type = 'text';
  searchInput.name = 'search';
  searchInput.className = 'ed-find-input';
  searchInput.setAttribute('main-field', 'true');
  searchInput.setAttribute('form', '');
  searchInput.spellcheck = false;
  searchInput.autocomplete = 'off';
  searchInput.value = query.search;
  findField.appendChild(searchInput);

  const makeOpt = (html: string, onClick: () => void) => {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'ed-opt';
    btn.innerHTML = html;
    btn.setAttribute('aria-pressed', 'false');
    btn.addEventListener('mousedown', (e) => e.preventDefault());
    btn.addEventListener('click', onClick);
    return btn;
  };

  const caseBtn = makeOpt('Aa', () => {
    caseSensitive = !caseSensitive;
    commit(true);
  });
  const wordBtn = makeOpt('<u>ab</u>', () => {
    wholeWord = !wholeWord;
    commit(true);
  });
  const reBtn = makeOpt('.*', () => {
    regexp = !regexp;
    commit(true);
  });
  findField.append(caseBtn, wordBtn, reBtn);

  // Row 1, Col 2: Count + divider + Prev / Next / Close
  const findActions = document.createElement('div');
  findActions.className = 'ed-find-acts';
  grid.appendChild(findActions);

  const countEl = document.createElement('span');
  countEl.className = 'ed-count';
  countEl.setAttribute('aria-live', 'polite');

  const sepEl = document.createElement('span');
  sepEl.className = 'ed-sep';

  const makeIconBtn = (name: string, pathD: string, onClick: () => void) => {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.name = name;
    btn.className = 'ed-ibtn';
    btn.appendChild(svgIcon(pathD, 14));
    btn.addEventListener('mousedown', (e) => e.preventDefault());
    btn.addEventListener('click', onClick);
    return btn;
  };

  const prevBtn = makeIconBtn('prev', PATH_UP, () => findPrevious(view));
  const nextBtn = makeIconBtn('next', PATH_DOWN, () => findNext(view));
  const closeBtn = makeIconBtn('close', PATH_CLOSE, () => closeSearchPanel(view));
  findActions.append(countEl, sepEl, prevBtn, nextBtn, closeBtn);

  // Row 2, Col 1: Replace input
  const replaceField = document.createElement('div');
  replaceField.className = 'ed-find-field ed-rep-cell';
  replaceField.hidden = true;
  grid.appendChild(replaceField);

  const replaceInput = document.createElement('input');
  replaceInput.type = 'text';
  replaceInput.name = 'replace';
  replaceInput.className = 'ed-find-input';
  replaceInput.setAttribute('form', '');
  replaceInput.spellcheck = false;
  replaceInput.autocomplete = 'off';
  replaceInput.value = query.replace;
  replaceField.appendChild(replaceInput);

  // Row 2, Col 2: Replace / Replace all buttons
  const replaceActions = document.createElement('div');
  replaceActions.className = 'ed-find-acts ed-rep-acts ed-rep-cell';
  replaceActions.hidden = true;
  grid.appendChild(replaceActions);

  const replaceBtn = document.createElement('button');
  replaceBtn.type = 'button';
  replaceBtn.name = 'replace';
  replaceBtn.className = 'ed-rbtn';
  replaceBtn.addEventListener('mousedown', (e) => e.preventDefault());
  replaceBtn.addEventListener('click', () => replaceNext(view));

  const replaceAllBtn = document.createElement('button');
  replaceAllBtn.type = 'button';
  replaceAllBtn.name = 'replaceAll';
  replaceAllBtn.className = 'ed-rbtn primary';
  replaceAllBtn.addEventListener('mousedown', (e) => e.preventDefault());
  replaceAllBtn.addEventListener('click', () => replaceAll(view));

  replaceActions.append(replaceBtn, replaceAllBtn);

  function syncLabels() {
    const alt = IS_MAC ? '⌥' : 'Alt+';
    const repKey = IS_MAC ? '⌘⌥F' : 'Ctrl+H';
    const repAllKey = IS_MAC ? '⌘⌥Enter' : 'Ctrl+Alt+Enter';
    searchInput.placeholder = tx('find');
    searchInput.setAttribute('aria-label', tx('find'));
    replaceInput.placeholder = tx('replaceWith');
    replaceInput.setAttribute('aria-label', tx('replaceWith'));

    toggleBtn.title = replaceOpen ? `${tx('hideReplace')} (${repKey})` : `${tx('toggleReplace')} (${repKey})`;
    toggleBtn.setAttribute('aria-label', replaceOpen ? tx('hideReplace') : tx('toggleReplace'));

    caseBtn.title = `${tx('matchCase')} (${alt}C)`;
    caseBtn.setAttribute('aria-label', tx('matchCase'));
    wordBtn.title = `${tx('matchWholeWord')} (${alt}W)`;
    wordBtn.setAttribute('aria-label', tx('matchWholeWord'));
    reBtn.title = `${tx('useRegularExpression')} (${alt}R)`;
    reBtn.setAttribute('aria-label', tx('useRegularExpression'));

    prevBtn.title = `${tx('previousMatch')} (Shift+Enter)`;
    prevBtn.setAttribute('aria-label', tx('previousMatch'));
    nextBtn.title = `${tx('nextMatch')} (Enter)`;
    nextBtn.setAttribute('aria-label', tx('nextMatch'));
    closeBtn.title = `${tx('close')} (Esc)`;
    closeBtn.setAttribute('aria-label', tx('close'));

    replaceBtn.textContent = tx('replace');
    replaceBtn.title = `${tx('replace')} (Enter)`;
    replaceAllBtn.textContent = tx('replaceAll');
    replaceAllBtn.title = `${tx('replaceAll')} (${repAllKey})`;
  }

  function syncReadOnly() {
    const ro = view.state.readOnly;
    toggleBtn.hidden = ro;
    box.classList.toggle('readonly', ro);
    if (ro && replaceOpen) replaceOpen = false;
    const showRep = !ro && replaceOpen;
    replaceField.hidden = !showRep;
    replaceActions.hidden = !showRep;
    toggleBtn.setAttribute('aria-expanded', String(showRep));
    toggleIconWrap.classList.toggle('open', showRep);
    syncLabels();
  }

  function syncOptionStates() {
    caseBtn.classList.toggle('on', caseSensitive);
    caseBtn.setAttribute('aria-pressed', String(caseSensitive));
    wordBtn.classList.toggle('on', wholeWord);
    wordBtn.setAttribute('aria-pressed', String(wholeWord));
    reBtn.classList.toggle('on', regexp);
    reBtn.setAttribute('aria-pressed', String(regexp));
  }

  function refreshMatches(forceRecount = false) {
    if (forceRecount || cachedDoc !== view.state.doc || !cachedQuery.eq(query)) {
      cachedDoc = view.state.doc;
      cachedQuery = query;
      cachedMatches = collectMatches(view, query);
    }
    const { items, capped } = cachedMatches;
    const hasSearch = query.search.length > 0;
    const invalid = hasSearch && !query.valid;
    const total = items.length;
    const totalText = capped ? `${total}+` : String(total);

    findField.classList.toggle('error', invalid || (hasSearch && total === 0));
    countEl.classList.toggle('empty', invalid || (hasSearch && total === 0));

    if (!hasSearch) {
      countEl.textContent = '';
    } else if (invalid) {
      countEl.textContent = tx('invalidRegex');
    } else if (total === 0) {
      countEl.textContent = tx('noResults');
    } else {
      const sel = view.state.selection.main;
      const idx = findMatchIndex(items, sel.from, sel.to);
      countEl.textContent =
        idx >= 0 ? tx('matchCount', { current: idx + 1, total: totalText }) : tx('matchTotalOnly', { total: totalText });
    }

    const noMatches = !hasSearch || invalid || total === 0;
    prevBtn.disabled = noMatches;
    nextBtn.disabled = noMatches;
    replaceBtn.disabled = noMatches || view.state.readOnly;
    replaceAllBtn.disabled = noMatches || view.state.readOnly;
  }

  function commit(jumpToMatch = false) {
    const nextQuery = new SearchQuery({
      search: searchInput.value,
      caseSensitive,
      wholeWord,
      regexp,
      replace: replaceInput.value,
      literal: query.literal,
    });
    const searchChanged =
      nextQuery.search !== query.search ||
      nextQuery.caseSensitive !== query.caseSensitive ||
      nextQuery.wholeWord !== query.wholeWord ||
      nextQuery.regexp !== query.regexp;

    if (!nextQuery.eq(query)) {
      query = nextQuery;
      syncOptionStates();
      view.dispatch({ effects: setSearchQuery.of(nextQuery) });
      if (jumpToMatch && searchChanged && nextQuery.search && nextQuery.valid) {
        const res = collectMatches(view, nextQuery);
        cachedDoc = view.state.doc;
        cachedQuery = nextQuery;
        cachedMatches = res;
        const hit = findFirstMatchAtOrAfter(res.items, searchAnchor);
        if (hit) {
          const sel = EditorSelection.single(hit.from, hit.to);
          view.dispatch({
            selection: sel,
            effects: scrollMatchIntoView(hit),
            userEvent: 'select.search',
          });
        }
      }
      refreshMatches(true);
    } else {
      syncOptionStates();
    }
  }

  function setReplace(open: boolean, focusReplace = false) {
    if (view.state.readOnly) return;
    replaceOpen = open;
    syncReadOnly();
    if (open && focusReplace) {
      if (searchInput.value) {
        replaceInput.focus();
        replaceInput.select();
      } else {
        searchInput.focus();
      }
    }
  }

  toggleBtn.addEventListener('mousedown', (e) => e.preventDefault());
  toggleBtn.addEventListener('click', () => setReplace(!replaceOpen, !replaceOpen));

  searchInput.addEventListener('input', () => commit(true));
  replaceInput.addEventListener('input', () => commit(false));

  box.addEventListener('keydown', (e) => {
    // Never intercept IME confirmation keys (macOS WebKit may fire keyCode 229 after compositionend)
    if (e.isComposing || e.keyCode === 229) return;

    if (e.altKey && !e.ctrlKey && !e.metaKey) {
      const k = e.key.toLowerCase();
      if (k === 'c') {
        e.preventDefault();
        caseSensitive = !caseSensitive;
        commit(true);
        return;
      }
      if (k === 'w') {
        e.preventDefault();
        wholeWord = !wholeWord;
        commit(true);
        return;
      }
      if (k === 'r') {
        e.preventDefault();
        regexp = !regexp;
        commit(true);
        return;
      }
    }

    if (runScopeHandlers(view, e, 'search-panel')) {
      e.preventDefault();
      return;
    }

    if (e.key === 'Enter') {
      if (e.target === searchInput) {
        e.preventDefault();
        (e.shiftKey ? findPrevious : findNext)(view);
      } else if (e.target === replaceInput) {
        e.preventDefault();
        if (e.altKey && (e.ctrlKey || e.metaKey)) {
          replaceAll(view);
        } else {
          replaceNext(view);
        }
      }
    }
  });

  syncOptionStates();
  syncReadOnly();
  refreshMatches(true);

  return {
    dom: host,
    top: true,
    mount() {
      searchInput.focus();
      searchInput.select();
    },
    update(u: ViewUpdate) {
      let queryUpdated = false;
      let wantReplace: boolean | null = null;
      for (const tr of u.transactions) {
        for (const eff of tr.effects) {
          if (eff.is(setSearchQuery) && !eff.value.eq(query)) {
            query = eff.value;
            caseSensitive = query.caseSensitive;
            wholeWord = query.wholeWord;
            regexp = query.regexp;
            searchInput.value = query.search;
            replaceInput.value = query.replace;
            queryUpdated = true;
          } else if (eff.is(setReplaceOpen)) {
            wantReplace = eff.value;
          }
        }
        if (tr.selection && !tr.isUserEvent('select.search')) {
          searchAnchor = tr.state.selection.main.from;
        }
      }

      const langChanged = isEn() !== lastLangEn;
      if (langChanged) {
        lastLangEn = isEn();
        syncLabels();
      }
      if (u.startState.readOnly !== u.state.readOnly) {
        syncReadOnly();
      }
      if (wantReplace !== null) {
        setReplace(wantReplace, wantReplace);
      }
      if (queryUpdated) {
        syncOptionStates();
      }
      if (queryUpdated || u.docChanged || u.selectionSet || langChanged || u.startState.readOnly !== u.state.readOnly) {
        refreshMatches(queryUpdated || u.docChanged);
      }
    },
  };
}

export function editorSearchExtension(): Extension {
  return search({
    top: true,
    createPanel: createEditorSearchPanel,
    scrollToMatch: (range) => scrollMatchIntoView(range),
  });
}

export const editorSearchKeymap: readonly KeyBinding[] = [
  ...searchKeymap,
  { key: 'Ctrl-h', run: openReplacePanel, scope: 'editor search-panel', preventDefault: true },
  { key: 'Mod-Alt-f', run: openReplacePanel, scope: 'editor search-panel', preventDefault: true },
];
