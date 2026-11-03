// Generated once by `i18next-cli types`, then edited (it is not overwritten): type options for t().
// resources.d.ts is regenerated from the Chinese files by `npm run i18n:check`.
import type Resources from './resources';

declare module 'i18next' {
  interface CustomTypeOptions {
    enableSelector: false;
    // Every t() comes from getT('<namespace>'), there is no default namespace
    defaultNS: false;
    // Unknown keys and missing / misspelled {{placeholders}} are type errors
    strictKeyChecks: true;
    strictInterpolationChecks: true;
    resources: Resources;
  }
}
