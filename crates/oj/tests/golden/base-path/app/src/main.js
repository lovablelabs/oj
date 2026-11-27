import logoUrl from "./logo.svg";
import bigUrl from "./big.txt?url";
import bigText from "./big.txt?raw";

document.querySelector("#logo").src = logoUrl;
console.log(bigUrl, bigText.length);
