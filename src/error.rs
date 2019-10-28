use crate::backtrace::Backtrace;
use crate::{Chain, Error};
use std::any::TypeId;
use std::error::Error as StdError;
use std::fmt::{self, Debug, Display};
use std::mem::{self, ManuallyDrop};
use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};

impl Error {
    /// Create a new error object from any error type.
    ///
    /// The error type must be threadsafe and `'static`, so that the `Error`
    /// will be as well.
    ///
    /// If the error type does not provide a backtrace, a backtrace will be
    /// created here to ensure that a backtrace exists.
    pub fn new<E>(error: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        let backtrace = backtrace_if_absent!(error);
        Error::from_std(error, backtrace)
    }

    pub(crate) fn from_std<E>(error: E, backtrace: Option<Backtrace>) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        let vtable = &ErrorVTable {
            object_drop: object_drop::<E>,
            object_ref: object_ref::<E>,
            object_mut: object_mut::<E>,
            object_boxed: object_boxed::<E>,
            object_downcast: object_downcast::<E>,
            object_drop_rest: object_drop_front::<E>,
        };

        // Safety: passing vtable that operates on the right type E.
        unsafe { Error::construct(error, vtable, backtrace) }
    }

    pub(crate) fn from_adhoc<M>(message: M, backtrace: Option<Backtrace>) -> Self
    where
        M: Display + Debug + Send + Sync + 'static,
    {
        let error: MessageError<M> = MessageError(message);
        let vtable = &ErrorVTable {
            object_drop: object_drop::<MessageError<M>>,
            object_ref: object_ref::<MessageError<M>>,
            object_mut: object_mut::<MessageError<M>>,
            object_boxed: object_boxed::<MessageError<M>>,
            object_downcast: object_downcast::<M>,
            object_drop_rest: object_drop_front::<M>,
        };

        // Safety: MessageError is repr(transparent) so it is okay for the
        // vtable to allow casting the MessageError<M> to M.
        unsafe { Error::construct(error, vtable, backtrace) }
    }

    pub(crate) fn from_display<M>(message: M, backtrace: Option<Backtrace>) -> Self
    where
        M: Display + Send + Sync + 'static,
    {
        let error: DisplayError<M> = DisplayError(message);
        let vtable = &ErrorVTable {
            object_drop: object_drop::<DisplayError<M>>,
            object_ref: object_ref::<DisplayError<M>>,
            object_mut: object_mut::<DisplayError<M>>,
            object_boxed: object_boxed::<DisplayError<M>>,
            object_downcast: object_downcast::<M>,
            object_drop_rest: object_drop_front::<M>,
        };

        // Safety: DisplayError is repr(transparent) so it is okay for the
        // vtable to allow casting the DisplayError<M> to M.
        unsafe { Error::construct(error, vtable, backtrace) }
    }

    pub(crate) fn from_context<C, E>(context: C, error: E, backtrace: Option<Backtrace>) -> Self
    where
        C: Display + Send + Sync + 'static,
        E: StdError + Send + Sync + 'static,
    {
        let error: ContextError<C, E> = ContextError { context, error };

        let vtable = &ErrorVTable {
            object_drop: object_drop::<ContextError<C, E>>,
            object_ref: object_ref::<ContextError<C, E>>,
            object_mut: object_mut::<ContextError<C, E>>,
            object_boxed: object_boxed::<ContextError<C, E>>,
            object_downcast: context_downcast::<C, E>,
            object_drop_rest: context_drop_rest::<C, E>,
        };

        // Safety: passing vtable that operates on the right type.
        unsafe { Error::construct(error, vtable, backtrace) }
    }

    // Takes backtrace as argument rather than capturing it here so that the
    // user sees one fewer layer of wrapping noise in the backtrace.
    //
    // Unsafe because the given vtable must have sensible behavior on the error
    // value of type E.
    unsafe fn construct<E>(
        error: E,
        vtable: &'static ErrorVTable,
        backtrace: Option<Backtrace>,
    ) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        let inner = Box::new(ErrorImpl {
            vtable,
            backtrace,
            _object: error,
        });
        let erased = mem::transmute::<Box<ErrorImpl<E>>, Box<ErrorImpl<()>>>(inner);
        let inner = ManuallyDrop::new(erased);
        Error { inner }
    }

    /// Wrap the error value with additional context.
    ///
    /// For attaching context to a `Result` as it is propagated, the
    /// [`Context`][crate::Context] extension trait may be more convenient than
    /// this function.
    ///
    /// The primary reason to use `error.context(...)` instead of
    /// `result.context(...)` via the `Context` trait would be if the context
    /// needs to depend on some data held by the underlying error:
    ///
    /// ```
    /// # use std::fmt::{self, Debug, Display};
    /// #
    /// # type T = ();
    /// #
    /// # impl std::error::Error for ParseError {}
    /// # impl Debug for ParseError {
    /// #     fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    /// #         unimplemented!()
    /// #     }
    /// # }
    /// # impl Display for ParseError {
    /// #     fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    /// #         unimplemented!()
    /// #     }
    /// # }
    /// #
    /// use anyhow::Result;
    /// use std::fs::File;
    /// use std::path::Path;
    ///
    /// struct ParseError {
    ///     line: usize,
    ///     column: usize,
    /// }
    ///
    /// fn parse_impl(file: File) -> Result<T, ParseError> {
    ///     # const IGNORE: &str = stringify! {
    ///     ...
    ///     # };
    ///     # unimplemented!()
    /// }
    ///
    /// pub fn parse(path: impl AsRef<Path>) -> Result<T> {
    ///     let file = File::open(&path)?;
    ///     parse_impl(file).map_err(|error| {
    ///         let context = format!(
    ///             "only the first {} lines of {} are valid",
    ///             error.line, path.as_ref().display(),
    ///         );
    ///         anyhow::Error::new(error).context(context)
    ///     })
    /// }
    /// ```
    pub fn context<C>(self, context: C) -> Self
    where
        C: Display + Send + Sync + 'static,
    {
        let error: ContextError<C, Error> = ContextError {
            context,
            error: self,
        };

        let vtable = &ErrorVTable {
            object_drop: object_drop::<ContextError<C, Error>>,
            object_ref: object_ref::<ContextError<C, Error>>,
            object_mut: object_mut::<ContextError<C, Error>>,
            object_boxed: object_boxed::<ContextError<C, Error>>,
            object_downcast: context_chain_downcast::<C>,
            object_drop_rest: context_chain_drop_rest::<C>,
        };

        // As the cause is anyhow::Error, we already have a backtrace for it.
        let backtrace = None;

        // Safety: passing vtable that operates on the right type.
        unsafe { Error::construct(error, vtable, backtrace) }
    }

    /// Get the backtrace for this Error.
    ///
    /// Backtraces are only available on the nightly channel. Tracking issue:
    /// [rust-lang/rust#53487][tracking].
    ///
    /// In order for the backtrace to be meaningful, the environment variable
    /// `RUST_LIB_BACKTRACE=1` must be defined. Backtraces are somewhat
    /// expensive to capture in Rust, so we don't necessarily want to be
    /// capturing them all over the place all the time.
    ///
    /// [tracking]: https://github.com/rust-lang/rust/issues/53487
    #[cfg(backtrace)]
    pub fn backtrace(&self) -> &Backtrace {
        self.inner.backtrace()
    }

    /// An iterator of the chain of source errors contained by this Error.
    ///
    /// This iterator will visit every error in the cause chain of this error
    /// object, beginning with the error that this error object was created
    /// from.
    ///
    /// # Example
    ///
    /// ```
    /// use anyhow::Error;
    /// use std::io;
    ///
    /// pub fn underlying_io_error_kind(error: &Error) -> Option<io::ErrorKind> {
    ///     for cause in error.chain() {
    ///         if let Some(io_error) = cause.downcast_ref::<io::Error>() {
    ///             return Some(io_error.kind());
    ///         }
    ///     }
    ///     None
    /// }
    /// ```
    pub fn chain(&self) -> Chain {
        self.inner.chain()
    }

    /// The lowest level cause of this error &mdash; this error's cause's
    /// cause's cause etc.
    ///
    /// The root cause is the last error in the iterator produced by
    /// [`chain()`][Error::chain].
    pub fn root_cause(&self) -> &(dyn StdError + 'static) {
        let mut chain = self.chain();
        let mut root_cause = chain.next().unwrap();
        for cause in chain {
            root_cause = cause;
        }
        root_cause
    }

    /// Returns `true` if `E` is the type wrapped by this error object.
    pub fn is<E>(&self) -> bool
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        self.downcast_ref::<E>().is_some()
    }

    /// Attempt to downcast the error object to a concrete type.
    pub fn downcast<E>(self) -> Result<E, Self>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        let target = TypeId::of::<E>();
        unsafe {
            let addr = match (self.inner.vtable.object_downcast)(&self.inner, target) {
                Some(addr) => addr,
                None => return Err(self),
            };
            let outer = ManuallyDrop::new(self);
            let error = ptr::read(addr.cast::<E>().as_ptr());
            let inner = ptr::read(&outer.inner);
            let erased = ManuallyDrop::into_inner(inner);
            (erased.vtable.object_drop_rest)(erased, target);
            Ok(error)
        }
    }

    /// Downcast this error object by reference.
    ///
    /// # Example
    ///
    /// ```
    /// # use anyhow::anyhow;
    /// # use std::fmt::{self, Display};
    /// # use std::task::Poll;
    /// #
    /// # #[derive(Debug)]
    /// # enum DataStoreError {
    /// #     Censored(()),
    /// # }
    /// #
    /// # impl Display for DataStoreError {
    /// #     fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    /// #         unimplemented!()
    /// #     }
    /// # }
    /// #
    /// # impl std::error::Error for DataStoreError {}
    /// #
    /// # const REDACTED_CONTENT: () = ();
    /// #
    /// # let error = anyhow!("...");
    /// # let root_cause = &error;
    /// #
    /// # let ret =
    /// // If the error was caused by redaction, then return a tombstone instead
    /// // of the content.
    /// match root_cause.downcast_ref::<DataStoreError>() {
    ///     Some(DataStoreError::Censored(_)) => Ok(Poll::Ready(REDACTED_CONTENT)),
    ///     None => Err(error),
    /// }
    /// # ;
    /// ```
    pub fn downcast_ref<E>(&self) -> Option<&E>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        let target = TypeId::of::<E>();
        unsafe {
            let addr = (self.inner.vtable.object_downcast)(&self.inner, target)?;
            Some(&*addr.cast::<E>().as_ptr())
        }
    }

    /// Downcast this error object by mutable reference.
    pub fn downcast_mut<E>(&mut self) -> Option<&mut E>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        let target = TypeId::of::<E>();
        unsafe {
            let addr = (self.inner.vtable.object_downcast)(&self.inner, target)?;
            Some(&mut *addr.cast::<E>().as_ptr())
        }
    }
}

impl<E> From<E> for Error
where
    E: StdError + Send + Sync + 'static,
{
    fn from(error: E) -> Self {
        let backtrace = backtrace_if_absent!(error);
        Error::from_std(error, backtrace)
    }
}

impl Deref for Error {
    type Target = dyn StdError + Send + Sync + 'static;

    fn deref(&self) -> &Self::Target {
        self.inner.error()
    }
}

impl DerefMut for Error {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.error_mut()
    }
}

impl Debug for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.inner.debug(formatter)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.inner.error())
    }
}

unsafe impl Send for Error {}
unsafe impl Sync for Error {}

impl Drop for Error {
    fn drop(&mut self) {
        unsafe {
            let inner = ptr::read(&self.inner);
            let erased = ManuallyDrop::into_inner(inner);
            (erased.vtable.object_drop)(erased);
        }
    }
}

struct ErrorVTable {
    object_drop: unsafe fn(Box<ErrorImpl<()>>),
    object_ref: unsafe fn(&ErrorImpl<()>) -> &(dyn StdError + Send + Sync + 'static),
    object_mut: unsafe fn(&mut ErrorImpl<()>) -> &mut (dyn StdError + Send + Sync + 'static),
    object_boxed: unsafe fn(Box<ErrorImpl<()>>) -> Box<dyn StdError + Send + Sync + 'static>,
    object_downcast: unsafe fn(&ErrorImpl<()>, TypeId) -> Option<NonNull<()>>,
    object_drop_rest: unsafe fn(Box<ErrorImpl<()>>, TypeId),
}

unsafe fn object_drop<E>(e: Box<ErrorImpl<()>>) {
    // Cast back to ErrorImpl<E> so that the allocator receives the correct
    // Layout to deallocate the Box's memory.
    let unerased = mem::transmute::<Box<ErrorImpl<()>>, Box<ErrorImpl<E>>>(e);
    drop(unerased);
}

unsafe fn object_drop_front<E>(e: Box<ErrorImpl<()>>, target: TypeId) {
    // Drop the fields of ErrorImpl other than E as well as the Box allocation,
    // without dropping E itself. This is used by downcast after doing a
    // ptr::read to take ownership of the E.
    let _ = target;
    let unerased = mem::transmute::<Box<ErrorImpl<()>>, Box<ErrorImpl<ManuallyDrop<E>>>>(e);
    drop(unerased);
}

unsafe fn object_ref<E>(e: &ErrorImpl<()>) -> &(dyn StdError + Send + Sync + 'static)
where
    E: StdError + Send + Sync + 'static,
{
    &(*(e as *const ErrorImpl<()> as *const ErrorImpl<E>))._object
}

unsafe fn object_mut<E>(e: &mut ErrorImpl<()>) -> &mut (dyn StdError + Send + Sync + 'static)
where
    E: StdError + Send + Sync + 'static,
{
    &mut (*(e as *mut ErrorImpl<()> as *mut ErrorImpl<E>))._object
}

unsafe fn object_boxed<E>(e: Box<ErrorImpl<()>>) -> Box<dyn StdError + Send + Sync + 'static>
where
    E: StdError + Send + Sync + 'static,
{
    mem::transmute::<Box<ErrorImpl<()>>, Box<ErrorImpl<E>>>(e)
}

unsafe fn object_downcast<E>(e: &ErrorImpl<()>, target: TypeId) -> Option<NonNull<()>>
where
    E: 'static,
{
    if TypeId::of::<E>() == target {
        let unerased = e as *const ErrorImpl<()> as *const ErrorImpl<E>;
        let addr = &(*unerased)._object as *const E as *mut ();
        Some(NonNull::new_unchecked(addr))
    } else {
        None
    }
}

unsafe fn context_downcast<C, E>(e: &ErrorImpl<()>, target: TypeId) -> Option<NonNull<()>>
where
    C: 'static,
    E: 'static,
{
    if TypeId::of::<C>() == target {
        let unerased = e as *const ErrorImpl<()> as *const ErrorImpl<ContextError<C, E>>;
        let addr = &(*unerased)._object.context as *const C as *mut ();
        Some(NonNull::new_unchecked(addr))
    } else if TypeId::of::<E>() == target {
        let unerased = e as *const ErrorImpl<()> as *const ErrorImpl<ContextError<C, E>>;
        let addr = &(*unerased)._object.error as *const E as *mut ();
        Some(NonNull::new_unchecked(addr))
    } else {
        None
    }
}

unsafe fn context_drop_rest<C, E>(e: Box<ErrorImpl<()>>, target: TypeId)
where
    C: 'static,
    E: 'static,
{
    // Called after downcasting by value to either the C or the E and doing a
    // ptr::read to take ownership of that value.
    if TypeId::of::<C>() == target {
        let unerased = mem::transmute::<
            Box<ErrorImpl<()>>,
            Box<ErrorImpl<ContextError<ManuallyDrop<C>, E>>>,
        >(e);
        drop(unerased);
    } else {
        let unerased = mem::transmute::<
            Box<ErrorImpl<()>>,
            Box<ErrorImpl<ContextError<C, ManuallyDrop<E>>>>,
        >(e);
        drop(unerased);
    }
}

unsafe fn context_chain_downcast<C>(e: &ErrorImpl<()>, target: TypeId) -> Option<NonNull<()>>
where
    C: 'static,
{
    if TypeId::of::<C>() == target {
        let unerased = e as *const ErrorImpl<()> as *const ErrorImpl<ContextError<C, Error>>;
        let addr = &(*unerased)._object.context as *const C as *mut ();
        Some(NonNull::new_unchecked(addr))
    } else {
        let unerased = e as *const ErrorImpl<()> as *const ErrorImpl<ContextError<C, Error>>;
        let source = &(*unerased)._object.error;
        (source.inner.vtable.object_downcast)(&source.inner, target)
    }
}

unsafe fn context_chain_drop_rest<C>(e: Box<ErrorImpl<()>>, target: TypeId)
where
    C: 'static,
{
    // Called after downcasting by value to either the C or one of the causes
    // and doing a ptr::read to take ownership of that value.
    if TypeId::of::<C>() == target {
        let unerased = mem::transmute::<
            Box<ErrorImpl<()>>,
            Box<ErrorImpl<ContextError<ManuallyDrop<C>, Error>>>,
        >(e);
        drop(unerased);
    } else {
        let unerased = mem::transmute::<
            Box<ErrorImpl<()>>,
            Box<ErrorImpl<ContextError<C, ManuallyDrop<Error>>>>,
        >(e);
        let inner = ptr::read(&unerased._object.error.inner);
        drop(unerased);
        let erased = ManuallyDrop::into_inner(inner);
        (erased.vtable.object_drop_rest)(erased, target);
    }
}

// repr C to ensure that E remains in the final position.
#[repr(C)]
pub(crate) struct ErrorImpl<E> {
    vtable: &'static ErrorVTable,
    backtrace: Option<Backtrace>,
    // NOTE: Don't use directly. Use only through vtable. Erased type may have
    // different alignment.
    _object: E,
}

// repr C to ensure that ContextError<C, E> has the same layout as
// ContextError<ManuallyDrop<C>, E> and ContextError<C, ManuallyDrop<E>>.
#[repr(C)]
pub(crate) struct ContextError<C, E> {
    pub context: C,
    pub error: E,
}

#[repr(transparent)]
struct MessageError<M>(M);

impl<M> Debug for MessageError<M>
where
    M: Display + Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<M> Display for MessageError<M>
where
    M: Display + Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl<M> StdError for MessageError<M> where M: Display + Debug + 'static {}

#[repr(transparent)]
struct DisplayError<M>(M);

impl<M> Debug for DisplayError<M>
where
    M: Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl<M> Display for DisplayError<M>
where
    M: Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl<M> StdError for DisplayError<M> where M: Display + 'static {}

impl<E> ErrorImpl<E> {
    fn erase(&self) -> &ErrorImpl<()> {
        unsafe { &*(self as *const ErrorImpl<E> as *const ErrorImpl<()>) }
    }
}

impl ErrorImpl<()> {
    fn error(&self) -> &(dyn StdError + Send + Sync + 'static) {
        unsafe { &*(self.vtable.object_ref)(self) }
    }

    fn error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
        unsafe { &mut *(self.vtable.object_mut)(self) }
    }

    #[cfg(backtrace)]
    fn backtrace(&self) -> &Backtrace {
        // This unwrap can only panic if the underlying error's backtrace method
        // is nondeterministic, which would only happen in maliciously
        // constructed code.
        self.backtrace
            .as_ref()
            .or_else(|| self.error().backtrace())
            .expect("backtrace capture failed")
    }

    fn chain(&self) -> Chain {
        Chain {
            next: Some(self.error()),
        }
    }

    fn debug(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "{}", self.error())?;

        let mut chain = self.chain().skip(1).enumerate().peekable();
        if let Some((n, error)) = chain.next() {
            write!(f, "\nCaused by:\n    ")?;
            if chain.peek().is_some() {
                write!(f, "{}: ", n)?;
            }
            writeln!(f, "{}", error)?;
            for (n, error) in chain {
                writeln!(f, "    {}: {}", n, error)?;
            }
        }

        #[cfg(backtrace)]
        {
            use std::backtrace::BacktraceStatus;

            let backtrace = self.backtrace();
            match backtrace.status() {
                BacktraceStatus::Captured => {
                    writeln!(f, "\n{}", backtrace)?;
                }
                BacktraceStatus::Disabled => {
                    writeln!(
                        f,
                        "\nBacktrace disabled; run with RUST_LIB_BACKTRACE=1 environment variable to display a backtrace"
                    )?;
                }
                _ => {}
            }
        }

        Ok(())
    }
}

impl<E> StdError for ErrorImpl<E>
where
    E: StdError,
{
    #[cfg(backtrace)]
    fn backtrace(&self) -> Option<&Backtrace> {
        Some(self.erase().backtrace())
    }

    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.erase().error().source()
    }
}

impl<E> Debug for ErrorImpl<E>
where
    E: Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.erase().debug(formatter)
    }
}

impl<E> Display for ErrorImpl<E>
where
    E: Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.erase().error(), formatter)
    }
}

impl From<Error> for Box<dyn StdError + Send + Sync + 'static> {
    fn from(error: Error) -> Self {
        let outer = ManuallyDrop::new(error);
        unsafe {
            let inner = ptr::read(&outer.inner);
            let erased = ManuallyDrop::into_inner(inner);
            (erased.vtable.object_boxed)(erased)
        }
    }
}

impl From<Error> for Box<dyn StdError + 'static> {
    fn from(error: Error) -> Self {
        Box::<dyn StdError + Send + Sync>::from(error)
    }
}

impl<'a> Iterator for Chain<'a> {
    type Item = &'a (dyn StdError + 'static);

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.next.take()?;
        self.next = next.source();
        Some(next)
    }
}
