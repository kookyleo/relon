fn cond() {}

fn gt() -> bool {
    false
}

enum ValOrFalse {
    Val(String),
    False,
}

fn gtt() -> ValOrFalse {
    ValOrFalse::False
}

fn lt() {}

fn eq() {}

fn els() {}
