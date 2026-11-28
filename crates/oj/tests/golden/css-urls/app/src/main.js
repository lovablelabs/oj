import "./main.css";
import sheetUrl from "./print.css?url";
import inlineSheet from "./theme.css?inline";

const link = document.createElement("link");
link.rel = "stylesheet";
link.media = "print";
link.href = sheetUrl;
document.head.append(link);
const style = document.createElement("style");
style.textContent = inlineSheet;
document.head.append(style);
