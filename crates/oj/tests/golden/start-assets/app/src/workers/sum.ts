self.onmessage = (e: MessageEvent<number[]>) => {
  self.postMessage(e.data.reduce((a, b) => a + b, 0));
};
