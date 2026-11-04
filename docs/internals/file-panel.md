# File panel

The session page's "workspace files" (`FilePanel.tsx`), also used standalone by Files sessions (see [sessions.md](sessions.md)), and the git / fs methods behind it.

## Layout

- With no file open it's a single tree column (`.file-panel.browse`, width `--tree-w`); opening a file widens it (`--panel-w`), with the editor on the left (tabs for opened files above it) and the tree on the right.
- The chat area keeps at least 420px, and the session header and input box collapse their text when narrow via container queries on `.session-view`.
- "View changes" is no longer a separate page: `SessionActions.showChanges` sends `OPEN_CHANGES`, and the session page opens the file panel on "Changes".
- "Add to chat" sends an `INSERT_COMPOSER` event, and the Composer appends `@relative-path` to the draft.

## Tree

- The tree marks changes (M / U / D) from `git.changes` (with `diff: false`, statuses only, because running git diff per file is too slow in big repos).
- The filter box uses `fs.search`.
- When a turn ends the tree refreshes, and open files without local edits are reread.
- Expanded folders are stored in localStorage `wp-exp-<host>/<cwd>` (missing ones are dropped quietly).
- Keyboard: VS Code's bindings (arrows, Home/End, Enter, F2, Delete) on a focus cursor separate from the open file; renaming a folder moves open tabs and expanded folders inside it.
- On Windows, `fs.list` must strip the `\\?\` prefix canonicalize adds (`fsapi::strip_verbatim`) before building child paths, otherwise child paths don't match the root and git paths (a "?" in the breadcrumbs, git marks lost); the frontend's `unverbatim` handles old records.

## Git: changes, diffs, history

- git operations are in `crates/yi-agentd/src/gitops.rs`. Paths are always relative to the repo root; absolute paths and `..` are rejected. Every git command runs with `GIT_OPTIONAL_LOCKS=0` (the panel polls status, which would otherwise take `index.lock` and break an agent's commit), and `git.changes` returns at most 10000 files (`total` is set when more were left out).
- Line counts `added` / `deleted` aren't affected by `diff: false`: tracked files are computed in one `git diff HEAD --numstat`, new files have their lines counted (null for binaries and files over 200KB). The Changes view shows +N −M to the right of each file and as a total at the top, and the diff of a single file is fetched with `paths` when it's opened.
- The Changes view lists only changed files, and an opened file can switch between "File / Changes" to see the diff. The panel doesn't offer commit / discard for now (yi-agentd's `git.commit` and `git.discard` are kept).
- Opened diffs (working tree vs HEAD, a commit vs its first parent, comparing two files) are full-file diffs like VS Code's (`MergeDiff.tsx`, `@codemirror/merge`: side by side or stacked, syntax highlighting, character-level marks, unchanged regions collapsed by default, which VS Code doesn't do). Both sides come from `git.blob` (`<rev>:<path>`, 1 MB limit like `fs.read`) and the working-tree side is the editor's draft, so unsaved edits show.
- Binary, oversized or unreadable sides, older daemons without `git.blob`, and tool snippets outside git fall back to the hunk-only `PaneDiff`. Before git status comes back, don't use the snippets recorded by tools in the session as a diff.
- Caching, like VS Code's Source Control view: the last git status per host + directory is cached in memory (`gitCache` in `FilePanel.tsx`), so a remounted panel shows it at once and refreshes quietly. The root path and top-level listing are cached the same way in `rootCache`, since the Changes list and the tree are laid out against the root. The commit history caches its first page and each commit's file list (`logCache` / `showCache` in `GitHistory.tsx`) and doesn't wait for git status (it only hides once status says the folder isn't a repo).

## Editor and tabs

- The editor is CodeMirror 6 (`CodeEditor.tsx`; folding, auto-closed brackets and VS Code's multi-cursor bindings). Language packs are lazy-loaded by file name through `@codemirror/language-data`, and highlight colors come from `--hl-*` variables on `.file-panel`.
- CodeMirror joins lines with `\n`, so the editor writes the draft back with the file's own line ending, and indentation is detected per file (`app/src/textFormat.ts`).
- `fs.read` flags files that aren't valid UTF-8 (`notUtf8`) and those open read-only, since saving the lossy text would corrupt them.
- Writing files goes through `fs.write`: saving sends the `modified` seen at read time, and if the agent changed the file meanwhile the write is refused and the user chooses between reloading from disk and overwriting. `fs.delete` only deletes files and empty folders.
- Open tabs and the current file are stored in localStorage `wp-open-<host>/<cwd>` and restored once the panel mounts and has the root (unreadable files are silently dropped).
- Double-clicking the empty tab bar or the empty editor area opens an untitled tab like VS Code's (`untitled:Untitled-N`, titled by its first line): its text lives only in memory (`untitledCache`, per panel key, so it survives remounts but not an app restart, unlike VS Code's hot exit), and saving asks for a path relative to the root (or absolute), writes with `createOnly` and asks before overwriting.
- The outline is `app/src/outline.ts` using per-extension regexes, no syntax tree.
- Markdown preview is `MdPreview` in `FilePreview.tsx`: it renders top-level blocks with an `<a class="md-ln" data-line>` anchor before each, and both split-view scroll sync and outline jumps interpolate between anchors; relative-path images become `data-src` and are fetched via `fs.read`.
- Images are fetched as base64 with `fs.read` and `image: true`.

## Search and replace

- Content search goes through `fs.grep` (file-by-file in Rust with the `regex` crate, no rg dependency; literal unless `regex`, plus `include` / `exclude` comma-separated globs with VS Code's semantics in `glob_set`, and `paths` for open files only).
- Replace follows VS Code's search view (`GrepPanel.tsx`): `fs.replace` shares `Matcher` with `fs.grep` so both see the same matches, replaces only the listed results (line + column), skips files whose `stat` from the search no longer matches, keeps every byte outside the matches, and returns the old text for one-step undo (written back with `fs.write`'s conflict check). The open file with unsaved edits is replaced in its draft via `dryRun` + `content`.

## Upload, download, open

- Uploads use `fs.upload`, downloads and PDFs use `fs.read` with `raw: true` (all limited to 50MB).
- `fs.open` (open with the system program / reveal in file manager) only accepts local connections, and the frontend only offers it for the local host.
- The Tauri window has `dragDropEnabled` off so HTML drag-and-drop gets system files.

## Browser tabs and HTML preview

- Browser tabs are in `Browser.tsx`, with tab keys `web:<url>` (`web:file:<absolute path>` is an HTML file on the host), stored in tabs like any other tab; remote localhost URLs go through `ports.forward` first.
- HTML previews go through `ClientHub::serve_preview`: `preview.root` exchanges a directory for a random token, and both the Tauri custom protocol `ahfs` and yi-web's `/fsv/<token>/<relative path>` map to `fs.read` raw, reading only the token's directory (.. rejected).
- The preview iframe has no allow-same-origin, and external URLs may not share the app's origin, so pages can't reach `hub_call`.
