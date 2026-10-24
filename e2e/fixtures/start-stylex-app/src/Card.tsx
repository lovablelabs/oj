import * as stylex from "@stylexjs/stylex";
import { colors } from "./tokens.stylex";

const fade = stylex.keyframes({
  from: { opacity: 0 },
  to: { opacity: 1 },
});

const styles = stylex.create({
  card: {
    backgroundColor: colors.surface,
    padding: 12,
    borderRadius: 4,
    animationName: fade,
    animationDuration: "1ms",
  },
  title: {
    color: "rgb(0, 0, 255)",
    fontWeight: 700,
  },
  active: {
    color: colors.accent,
  },
  button: {
    color: { default: "rgb(0, 0, 0)", ":hover": "rgb(255, 0, 0)" },
  },
});

export function Card({ active, onToggle }: { active: boolean; onToggle: () => void }) {
  return (
    <section id="card" {...stylex.props(styles.card)}>
      <h1 id="title" {...stylex.props(styles.title, active && styles.active)}>StyleX</h1>
      <button id="toggle" onClick={onToggle} {...stylex.props(styles.button)}>
        toggle
      </button>
    </section>
  );
}
