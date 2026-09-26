import { rootRoute } from "./routes/__root";
import { indexRoute } from "./routes/index";

export const routeTree = rootRoute.addChildren([indexRoute]);
