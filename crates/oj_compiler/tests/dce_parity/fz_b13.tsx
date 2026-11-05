import { createServerOnlyFn } from "@tanstack/react-start";
import { Icon } from "./m";
export function C() {
  const X = () => <Icon />;
  return <div>{createServerOnlyFn(/*<*/() => <X />/*>*/)}</div>;
}
