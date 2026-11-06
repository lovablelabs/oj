#[derive(Clone, Debug)]
pub struct Stack<T> {
    data: Vec<T>,
}

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Stack { data: Vec::new() }
    }
}

impl<T> Stack<T> {
    pub fn push(&mut self, item: T) {
        self.data.push(item);
    }

    pub fn pop(&mut self) -> T {
        match self.data.pop() {
            Some(item) => item,
            None => panic!("stack is empty"),
        }
    }

    pub fn peek(&self) -> &T {
        match self.data.last() {
            Some(item) => item,
            None => panic!("stack is empty"),
        }
    }
}
