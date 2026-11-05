// Translation resources, one JSON file per namespace and language.
// Chinese is the reference: its files define the keys, and the TypeScript types of t() (resources.d.ts).
// Every other language must have the same keys and {{placeholders}}; `npm run i18n:check` (part of the build) checks this.
// To add a namespace: create zh/<ns>.json and en/<ns>.json and list them below.

import zhApp from './zh/app.json';
import zhAppUpdate from './zh/appUpdate.json';
import zhBack from './zh/back.json';
import zhBgTasks from './zh/bgTasks.json';
import zhBrowser from './zh/browser.json';
import zhChatImages from './zh/chatImages.json';
import zhChatView from './zh/chatView.json';
import zhComputerUse from './zh/computerUse.json';
import zhMcpSettings from './zh/mcpSettings.json';
import zhComposer from './zh/composer.json';
import zhDiffView from './zh/diffView.json';
import zhFilePanel from './zh/filePanel.json';
import zhFilePreview from './zh/filePreview.json';
import zhFilesView from './zh/filesView.json';
import zhGitHistory from './zh/gitHistory.json';
import zhGrepPanel from './zh/grepPanel.json';
import zhHandoff from './zh/handoff.json';
import zhHandoffCard from './zh/handoffCard.json';
import zhHistoryView from './zh/historyView.json';
import zhHome from './zh/home.json';
import zhHostTools from './zh/hostTools.json';
import zhHostsView from './zh/hostsView.json';
import zhImageMenu from './zh/imageMenu.json';
import zhLightbox from './zh/lightbox.json';
import zhLimit from './zh/limit.json';
import zhLimitDock from './zh/limitDock.json';
import zhMain from './zh/main.json';
import zhMermaidBlock from './zh/mermaidBlock.json';
import zhModelPicker from './zh/modelPicker.json';
import zhNewSession from './zh/newSession.json';
import zhOverlay from './zh/overlay.json';
import zhPalette from './zh/palette.json';
import zhPlanDock from './zh/planDock.json';
import zhPortsView from './zh/portsView.json';
import zhPrefsView from './zh/prefsView.json';
import zhQrScanner from './zh/qrScanner.json';
import zhQuestionCard from './zh/questionCard.json';
import zhQuickOpen from './zh/quickOpen.json';
import zhSearchView from './zh/searchView.json';
import zhSessionActions from './zh/sessionActions.json';
import zhSessionView from './zh/sessionView.json';
import zhSettingsView from './zh/settingsView.json';
import zhSidebar from './zh/sidebar.json';
import zhStatsView from './zh/statsView.json';
import zhStatusLine from './zh/statusLine.json';
import zhStore from './zh/store.json';
import zhTermPanel from './zh/termPanel.json';
import zhTerminalView from './zh/terminalView.json';
import zhTitleBar from './zh/titleBar.json';
import zhTransport from './zh/transport.json';
import zhUpgrade from './zh/upgrade.json';
import zhUsageDialog from './zh/usageDialog.json';
import zhUtil from './zh/util.json';
import enApp from './en/app.json';
import enAppUpdate from './en/appUpdate.json';
import enBack from './en/back.json';
import enBgTasks from './en/bgTasks.json';
import enBrowser from './en/browser.json';
import enChatImages from './en/chatImages.json';
import enChatView from './en/chatView.json';
import enComputerUse from './en/computerUse.json';
import enMcpSettings from './en/mcpSettings.json';
import enComposer from './en/composer.json';
import enDiffView from './en/diffView.json';
import enFilePanel from './en/filePanel.json';
import enFilePreview from './en/filePreview.json';
import enFilesView from './en/filesView.json';
import enGitHistory from './en/gitHistory.json';
import enGrepPanel from './en/grepPanel.json';
import enHandoff from './en/handoff.json';
import enHandoffCard from './en/handoffCard.json';
import enHistoryView from './en/historyView.json';
import enHome from './en/home.json';
import enHostTools from './en/hostTools.json';
import enHostsView from './en/hostsView.json';
import enImageMenu from './en/imageMenu.json';
import enLightbox from './en/lightbox.json';
import enLimit from './en/limit.json';
import enLimitDock from './en/limitDock.json';
import enMain from './en/main.json';
import enMermaidBlock from './en/mermaidBlock.json';
import enModelPicker from './en/modelPicker.json';
import enNewSession from './en/newSession.json';
import enOverlay from './en/overlay.json';
import enPalette from './en/palette.json';
import enPlanDock from './en/planDock.json';
import enPortsView from './en/portsView.json';
import enPrefsView from './en/prefsView.json';
import enQrScanner from './en/qrScanner.json';
import enQuestionCard from './en/questionCard.json';
import enQuickOpen from './en/quickOpen.json';
import enSearchView from './en/searchView.json';
import enSessionActions from './en/sessionActions.json';
import enSessionView from './en/sessionView.json';
import enSettingsView from './en/settingsView.json';
import enSidebar from './en/sidebar.json';
import enStatsView from './en/statsView.json';
import enStatusLine from './en/statusLine.json';
import enStore from './en/store.json';
import enTermPanel from './en/termPanel.json';
import enTerminalView from './en/terminalView.json';
import enTitleBar from './en/titleBar.json';
import enTransport from './en/transport.json';
import enUpgrade from './en/upgrade.json';
import enUsageDialog from './en/usageDialog.json';
import enUtil from './en/util.json';

export const resources = {
  zh: {
    app: zhApp,
    appUpdate: zhAppUpdate,
    back: zhBack,
    bgTasks: zhBgTasks,
    browser: zhBrowser,
    chatImages: zhChatImages,
    chatView: zhChatView,
    computerUse: zhComputerUse,
    mcpSettings: zhMcpSettings,
    composer: zhComposer,
    diffView: zhDiffView,
    filePanel: zhFilePanel,
    filePreview: zhFilePreview,
    filesView: zhFilesView,
    gitHistory: zhGitHistory,
    grepPanel: zhGrepPanel,
    handoff: zhHandoff,
    handoffCard: zhHandoffCard,
    historyView: zhHistoryView,
    home: zhHome,
    hostTools: zhHostTools,
    hostsView: zhHostsView,
    imageMenu: zhImageMenu,
    lightbox: zhLightbox,
    limit: zhLimit,
    limitDock: zhLimitDock,
    main: zhMain,
    mermaidBlock: zhMermaidBlock,
    modelPicker: zhModelPicker,
    newSession: zhNewSession,
    overlay: zhOverlay,
    palette: zhPalette,
    planDock: zhPlanDock,
    portsView: zhPortsView,
    prefsView: zhPrefsView,
    qrScanner: zhQrScanner,
    questionCard: zhQuestionCard,
    quickOpen: zhQuickOpen,
    searchView: zhSearchView,
    sessionActions: zhSessionActions,
    sessionView: zhSessionView,
    settingsView: zhSettingsView,
    sidebar: zhSidebar,
    statsView: zhStatsView,
    statusLine: zhStatusLine,
    store: zhStore,
    termPanel: zhTermPanel,
    terminalView: zhTerminalView,
    titleBar: zhTitleBar,
    transport: zhTransport,
    upgrade: zhUpgrade,
    usageDialog: zhUsageDialog,
    util: zhUtil,
  },
  en: {
    app: enApp,
    appUpdate: enAppUpdate,
    back: enBack,
    bgTasks: enBgTasks,
    browser: enBrowser,
    chatImages: enChatImages,
    chatView: enChatView,
    computerUse: enComputerUse,
    mcpSettings: enMcpSettings,
    composer: enComposer,
    diffView: enDiffView,
    filePanel: enFilePanel,
    filePreview: enFilePreview,
    filesView: enFilesView,
    gitHistory: enGitHistory,
    grepPanel: enGrepPanel,
    handoff: enHandoff,
    handoffCard: enHandoffCard,
    historyView: enHistoryView,
    home: enHome,
    hostTools: enHostTools,
    hostsView: enHostsView,
    imageMenu: enImageMenu,
    lightbox: enLightbox,
    limit: enLimit,
    limitDock: enLimitDock,
    main: enMain,
    mermaidBlock: enMermaidBlock,
    modelPicker: enModelPicker,
    newSession: enNewSession,
    overlay: enOverlay,
    palette: enPalette,
    planDock: enPlanDock,
    portsView: enPortsView,
    prefsView: enPrefsView,
    qrScanner: enQrScanner,
    questionCard: enQuestionCard,
    quickOpen: enQuickOpen,
    searchView: enSearchView,
    sessionActions: enSessionActions,
    sessionView: enSessionView,
    settingsView: enSettingsView,
    sidebar: enSidebar,
    statsView: enStatsView,
    statusLine: enStatusLine,
    store: enStore,
    termPanel: enTermPanel,
    terminalView: enTerminalView,
    titleBar: enTitleBar,
    transport: enTransport,
    upgrade: enUpgrade,
    usageDialog: enUsageDialog,
    util: enUtil,
  },
} as const;
