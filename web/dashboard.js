// Bootstrap only. Everything the page does lives in dashboard-app.js, which is
// a function of its context so a test can supply one — see tests/dashboard.test.mjs.
// The session gate runs first: nothing paints until the API vouches for us.
import { loadRuntime } from './theme.js';
import { requireSession, installUnauthorizedTrap } from './auth.js';
import { startDashboard } from './dashboard-app.js';

const { brand, locale, dict } = await loadRuntime();
await requireSession(dict);
installUnauthorizedTrap();
await startDashboard({ brand, locale, dict });
