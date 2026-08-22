const DARK = 'dark';
const LIGHT = 'light';
const AUTO = 'auto';
const MODES = [AUTO, DARK, LIGHT] as const;
const MODE_LABELS = {
  [AUTO]: 'automatic',
  [DARK]: 'dark',
  [LIGHT]: 'light',
} as const;

type Mode = (typeof MODES)[number];

const preference = matchMedia('(prefers-color-scheme: dark)');
let sessionMode: Mode = AUTO;

const getMode = (): Mode => {
  try {
    const stored = localStorage.getItem(DARK);
    sessionMode = MODES.includes(stored as Mode) ? (stored as Mode) : AUTO;
    return sessionMode;
  } catch {
    return sessionMode;
  }
};

const setMode = (mode: Mode) => {
  sessionMode = mode;
  try {
    localStorage.setItem(DARK, mode);
  } catch {
    // The visual preference can remain session-only when storage is blocked.
  }
};

const updateTheme = () => {
  const mode = getMode();
  const isDark = mode === DARK || (mode === AUTO && preference.matches);
  document.documentElement.classList.toggle(DARK, isDark);
  document.documentElement.classList.toggle(LIGHT, !isDark);

  const toggle = document.querySelector<HTMLButtonElement>('#dark');
  if (toggle != null) {
    toggle.className = mode;
    const nextMode = MODES[(MODES.indexOf(mode) + 1) % MODES.length] ?? AUTO;
    toggle.title = `Color theme: ${MODE_LABELS[mode]}`;
    toggle.setAttribute(
      'aria-label',
      `Color theme: ${MODE_LABELS[mode]}; activate for ${MODE_LABELS[nextMode]}`,
    );
  }
};

const cycleTheme = () => {
  const mode = getMode();
  const nextMode = MODES[(MODES.indexOf(mode) + 1) % MODES.length] ?? AUTO;
  setMode(nextMode);
  updateTheme();
};

preference.addEventListener('change', updateTheme);
window.addEventListener('storage', (event) => {
  if (event.storageArea === localStorage && event.key === DARK) {
    updateTheme();
  }
});

window.addEventListener('DOMContentLoaded', () => {
  const toggle = document.querySelector<HTMLButtonElement>('#dark');
  toggle?.addEventListener('click', cycleTheme);

  updateTheme();
});

updateTheme();
