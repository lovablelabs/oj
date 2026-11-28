import styles from "./card.module.css";
import unused from "./unused.module.css";
import icon from "./icon.svg?inline";
import { t } from "./i18n/strings.js";

console.log(styles.cardTitle, styles["card-title"], styles.spin, t("hi"), icon);
import("./widget.gen.js").then((m) => m.widget());
