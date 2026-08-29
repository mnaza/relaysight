export async function loadRuntime() {
  const baseBrand = await fetch('brand.json', { cache: 'no-store' }).then(r => {
    if (!r.ok) throw new Error('brand.json failed');
    return r.json();
  });

  const storedOverride = JSON.parse(localStorage.getItem('brandOverride') || 'null');
  const brand = storedOverride ? mergeBrand(baseBrand, storedOverride) : baseBrand;

  const requested = new URLSearchParams(location.search).get('lang');
  const stored = localStorage.getItem('locale');
  const browser = (navigator.language || '').split('-')[0];
  const supported = brand.supportedLocales || ['en'];
  const locale = [requested, stored, browser, brand.defaultLocale, 'en']
    .find(value => value && supported.includes(value)) || supported[0];

  const dict = await fetch(`locales/${locale}.json`, { cache: 'no-store' }).then(r => r.json());
  applyBrand(brand);
  return { brand, locale, dict };
}

export function applyBrand(brand) {
  const root = document.documentElement;
  const theme = brand.theme || {};
  const vars = {
    '--primary': theme.primary,
    '--accent': theme.accent,
    '--background': theme.background,
    '--surface': theme.surface,
    '--text': theme.text,
    '--muted': theme.muted,
    '--radius': `${theme.radius || 18}px`,
  };
  Object.entries(vars).forEach(([key, value]) => value && root.style.setProperty(key, value));
  document.title = brand.name;

  if (brand.faviconUrl) {
    let favicon = document.querySelector('link[rel="icon"]');
    if (!favicon) {
      favicon = document.createElement('link');
      favicon.rel = 'icon';
      document.head.appendChild(favicon);
    }
    favicon.href = brand.faviconUrl;
  }

  if (brand.customCssUrl && !document.querySelector('[data-brand-css]')) {
    const link = document.createElement('link');
    link.rel = 'stylesheet';
    link.href = brand.customCssUrl;
    link.dataset.brandCss = 'true';
    document.head.appendChild(link);
  }
}

export function t(dict, key, fallback = key, vars = {}) {
  const value = String(dict[key] ?? fallback);
  return value.replace(/\{([A-Za-z0-9_]+)\}/g, (match, name) => name in vars ? String(vars[name]) : match);
}

export function brandMark(brand, compact = false) {
  const wrap = document.createElement('a');
  wrap.className = 'brand';
  wrap.href = '/';

  if (brand.logoUrl) {
    const image = document.createElement('img');
    image.className = 'brand-logo-image';
    image.alt = brand.name;
    image.src = brand.logoUrl;
    wrap.appendChild(image);
  } else {
    const mark = document.createElement('span');
    mark.className = 'brand-mark';
    mark.textContent = (brand.name || 'V').slice(0, 1).toUpperCase();
    wrap.appendChild(mark);
  }

  if (!compact) {
    const name = document.createElement('strong');
    name.textContent = brand.name;
    wrap.appendChild(name);
  }
  return wrap;
}

export function localeSelect(brand, locale) {
  const select = document.createElement('select');
  select.className = 'locale-select';
  select.setAttribute('aria-label', 'Language');
  (brand.supportedLocales || []).forEach(code => {
    const option = document.createElement('option');
    option.value = code;
    option.textContent = code.toUpperCase();
    option.selected = code === locale;
    select.appendChild(option);
  });
  select.addEventListener('change', () => {
    localStorage.setItem('locale', select.value);
    const url = new URL(location.href);
    url.searchParams.set('lang', select.value);
    location.href = url.toString();
  });
  return select;
}

export function mergeBrand(base, override) {
  return {
    ...base,
    ...override,
    theme: { ...(base.theme || {}), ...(override.theme || {}) },
    freeTier: { ...(base.freeTier || {}), ...(override.freeTier || {}) },
    gateway: { ...(base.gateway || {}), ...(override.gateway || {}) },
    contact: { ...(base.contact || {}), ...(override.contact || {}) },
  };
}
