console.log(__APP_VERSION__, __FLAGS__.beta, import.meta.env.VITE_API, process.env.NODE_ENV);
if (process.env.NODE_ENV !== "production") console.log("development only");
console.log(typeof __UNDEFINED_DEFINE__, import.meta.env.SSR);
