use open_coroutine_examples::sleep_test_co;

// #[open_coroutine::main(event_loop_size = 2,
//                        stack_size = 65536,
//                        min_size = 0,
//                        max_size = 2,
//                        keep_alive_time = 0)]
#[open_coroutine::main]
fn main() {
    sleep_test_co(1);
    sleep_test_co(1000);
}
