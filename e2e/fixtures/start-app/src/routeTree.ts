import { rootRoute } from "./routes/__root";
import { indexRoute } from "./routes/index";
import { aboutRoute } from "./routes/about";
import { userRoute } from "./routes/user";
import { exportRoute } from "./routes/export";
import { requestUrlRoute } from "./routes/request-url";
import { dunderRoute } from "./routes/dunder";
import { resolveErrorRoute } from "./routes/resolve-error";

// A code-based route tree (no file-based codegen), so the fixture has no
// generated files to keep in sync.
export const routeTree = rootRoute.addChildren([indexRoute, aboutRoute, userRoute, exportRoute, requestUrlRoute, dunderRoute, resolveErrorRoute]);
