export const DEFAULT_THEME_ID = 'monokai';
export const THEME_STORAGE_KEY = 'amte.theme.v1';
export const REQUIRED_CSS_ROLES = ['workspace', 'paper', 'card', 'ink', 'muted', 'border', 'selection', 'caret', 'accent', 'codeSurface', 'code', 'blockquote', 'pageEdge'];
export const REQUIRED_SYNTAX_ROLES = ['headingMarker', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'marker', 'emphasis', 'link', 'code', 'blockquote', 'list', 'strikethrough'];

const COLOR_VALUE = /^#[0-9a-f]{6}(?:[0-9a-f]{2})?$/i;
const toKebab = (value) => value.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`);

export function validateTheme(theme) {
  if (!theme || typeof theme !== 'object' || !theme.colors || !theme.roles?.css || !theme.roles?.syntax) throw new Error('Theme must provide colors and CSS/syntax roles.');
  for (const [group, roles] of [['css', REQUIRED_CSS_ROLES], ['syntax', REQUIRED_SYNTAX_ROLES]]) {
    for (const role of roles) {
      const color = theme.colors[theme.roles[group][role]];
      if (!color || !COLOR_VALUE.test(color.value)) throw new Error(`Theme ${group} role "${role}" is missing a valid color.`);
    }
  }
  return theme;
}

export function getStoredThemeId(storage = globalThis.localStorage) {
  try {
    return storage?.getItem(THEME_STORAGE_KEY) || DEFAULT_THEME_ID;
  } catch {
    return DEFAULT_THEME_ID;
  }
}

export function persistThemeId(themeId, storage = globalThis.localStorage) {
  try { storage?.setItem(THEME_STORAGE_KEY, themeId); } catch { /* Preferences are optional. */ }
}

export function chooseTheme(themes, requestedId) {
  return themes.get(requestedId) || themes.get(DEFAULT_THEME_ID) || null;
}

function setPresentationVariables(root, presentation, colors) {
  const names = [];
  for (const [section, values] of Object.entries(presentation || {})) {
    if (!values || typeof values !== 'object') continue;
    for (const [propertyName, rawValue] of Object.entries(values)) {
      const value = typeof rawValue === 'string' && colors[rawValue]?.value ? colors[rawValue].value : rawValue;
      if (typeof value === 'string' || typeof value === 'number') {
        const name = `--theme-${toKebab(section)}-${toKebab(propertyName)}`;
        root.style.setProperty(name, String(value));
        names.push(name);
      }
    }
  }
  root.dataset.themePresentationVars = names.join(' ');
}

export function applyTheme(themeId, theme, root = document.documentElement) {
  for (const name of (root.dataset.themePresentationVars || '').split(' ')) if (name) root.style.removeProperty(name);
  for (const [group, roles] of Object.entries(theme.roles)) {
    for (const [role, colorName] of Object.entries(roles)) {
      root.style.setProperty(`--theme-${group === 'syntax' ? 'syntax-' : ''}${toKebab(role)}`, theme.colors[colorName].value);
    }
  }
  setPresentationVariables(root, theme.presentation, theme.colors);
  root.dataset.theme = themeId;
}

export function isThemeToggleShortcut(event) {
  return event.ctrlKey && !event.altKey && !event.metaKey && String(event.key || '').toLowerCase() === 't';
}
