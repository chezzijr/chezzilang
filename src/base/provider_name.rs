//! base: the names of synthesized default providers, and how a message shows them.

/// Name prefix of a synthesized **default-argument provider** — the hidden zero-arg function that
/// evaluates one parameter/field default in the module that DECLARES it (W7-51). `$` is unspellable
/// in Chezzi source, so a provider name can never collide with a user global or an import bind.
pub const PROVIDER_PREFIX: &str = "$def$";

/// Which kind of slot a provider was synthesized for. The name embeds it because `owner.param` alone
/// is NOT injective: a struct field (`struct S: m: int = g()`, owner `S`, param `m`) and a same-named
/// free function's parameter (`fn S(m: int = g())`, owner `S`, param `m`) produced the SAME name, and
/// both declarations are legal today — measured on `b1307258` the pair type-checked clean, and with
/// one name for both providers the checker reported `function '$def$2$S.m$' is already defined`
/// (leaking an internal symbol into `--errors=json`, i.e. the editor squiggle). A method's owner
/// carries a `.` (`S.m`) and a struct/fn name never can, so within one kind the name is injective.
#[derive(Clone, Copy, PartialEq)]
pub enum Slot {
    /// A parameter of a free fn or a method.
    Param,
    /// A struct field.
    Field,
}

impl Slot {
    /// One character, so the name stays short and stays unspellable.
    pub fn tag(self) -> char {
        match self {
            Slot::Param => 'p',
            Slot::Field => 'f',
        }
    }
}

/// The provider function's name for one parameter/field default. `file` is the DECLARING module's
/// `resolver::LoadedModule::file` id (already unique per module, and the same coordinate
/// the checker→compiler side-table keys use), `slot` says parameter vs struct field (see [`Slot`]),
/// `owner` names the declaring callable (`f`, `S` for a struct field, `S.m` for a method). ONE
/// function, called by the synthesizer, every registry collector and the checker's decl-site `?`
/// gate (each passing the result straight into [`dflt_for`]), so they can never drift into naming a
/// provider that does not exist.
pub fn provider_name(file: u32, slot: Slot, owner: &str, param: &str) -> String {
    format!("{PROVIDER_PREFIX}{file}${}${owner}.{param}$", slot.tag())
}

/// [`provider_name`] for a **parameter** slot, for the checker's decl-site `?` gate: it asks whether
/// the default it is about to infer will be judged again inside a provider body, and the honest way
/// to answer is to look for the function [`synthesize_providers`] would have emitted.
pub fn param_provider_name(file: u32, owner: &str, param: &str) -> String {
    provider_name(file, Slot::Param, owner, param)
}

/// Render a compiled function's name for a user-visible message — a stack-trace frame, chiefly.
/// A synthesized provider's internal name is unspellable ON PURPOSE (`$def$2$f.x$`), which also
/// makes it unreadable, so a frame for one is shown as what it is. Every other name passes through
/// borrowed, so this is free on the ordinary path.
pub fn display_fn_name(name: &str) -> std::borrow::Cow<'_, str> {
    if name.starts_with(PROVIDER_PREFIX) {
        std::borrow::Cow::Owned(format!("<default for {}>", provider_label(name)))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// Decode a provider name back into a human phrase for a diagnostic (`'x' of 'f'`), which every
/// caller prefixes with its own "the default for …". Deliberately noun-free: the name does not
/// record whether the slot is a **parameter** or a struct **field**, and calling a field a parameter
/// was measurable (`struct S: n: int = S().n` reported `the default value for parameter 'n' of 'S'
/// is cyclic`). Total: an unparseable name (impossible for one [`provider_name`] built) degrades to
/// itself.
pub fn provider_label(name: &str) -> String {
    let Some(rest) = name
        .strip_prefix(PROVIDER_PREFIX)
        .and_then(|r| r.strip_suffix('$'))
    else {
        return format!("'{name}'");
    };
    // `<file>$<slot>$<owner>.<param>` — the owner may itself contain a `.` (`S.m`), the param never
    // does; `<file>` and `<slot>` are both internal coordinates and neither is shown.
    let Some((_, rest)) = rest.split_once('$') else {
        return format!("'{name}'");
    };
    let Some((_, owner_param)) = rest.split_once('$') else {
        return format!("'{name}'");
    };
    match owner_param.rsplit_once('.') {
        Some((owner, param)) => format!("'{param}' of '{owner}'"),
        None => format!("'{owner_param}'"),
    }
}
