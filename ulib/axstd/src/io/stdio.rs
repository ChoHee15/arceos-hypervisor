/// Prints to the standard output.
///
/// Equivalent to the [`println!`] macro except that a newline is not printed at
/// the end of the message.
///
/// [`println!`]: crate::println
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::io::__print_impl(format_args!($($arg)*));
    }
}

/// Prints to the standard output, with a newline.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {
        $crate::io::__print_impl(format_args!("{}\n", format_args!($($arg)*)));
    }
}

#[doc(hidden)]
pub fn __print_impl(args: core::fmt::Arguments) {
    // synchronize using the lock in axlog, to avoid interleaving
    // with kernel logs
    arceos_api::stdio::ax_console_write_fmt(args).unwrap();
}
