import { defineTheme, hex, themes, type Theme } from "@profullstack/hqtui";

/** c0mpute.com's look: black, one green accent, mono type (DIP-0008). */
export const c0mputeTheme: Theme = defineTheme(
  {
    name: "c0mpute",
    background: hex("#000000"),
    surface: hex("#0a0f0a"),
    foreground: hex("#d8e2d8"),
    muted: hex("#5f7a5f"),
    primary: hex("#22e56a"),
    secondary: hex("#3ddc84"),
    accent: hex("#5bd6ff"),
    success: hex("#22e56a"),
    warning: hex("#ffcb6b"),
    danger: hex("#ff5370"),
    info: hex("#5bd6ff"),
    border: hex("#1c2a1c"),
    borderFocused: hex("#22e56a"),
    title: hex("#22e56a"),
    selection: hex("#14241a"),
    selectionText: hex("#d8e2d8"),
    graph: [hex("#22e56a"), hex("#5bd6ff"), hex("#ffcb6b"), hex("#ff5370")],
  },
  themes.dark,
);
