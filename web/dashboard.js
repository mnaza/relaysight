// Bootstrap only. Everything the page does lives in dashboard-app.js, which is
// a function of its context so a test can supply one — see tests/dashboard.test.mjs.
import { loadRuntime } from './theme.js';
import { startDashboard } from './dashboard-app.js';

const { brand, locale, dict } = await loadRuntime();
await startDashboard({ brand, locale, dict });
