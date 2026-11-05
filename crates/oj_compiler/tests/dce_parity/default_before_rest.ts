export function make(obj: any) {
  const { a = 1, ...rest } = obj;
  return [rest, keep(/*<*/() => a/*>*/)];
}
