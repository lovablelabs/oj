export function make(obj: any) {
  const {
    ...rest
  } = obj;
  return [rest, keep(undefined)];
}
