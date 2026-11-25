use hubdouble_render::{Repository, SimpleUser};
use serde::Serialize;
use serde::ser::{Error as _, Impossible, SerializeMap, Serializer};

const NOT_A_MAP: &str = "the push repository writes only a map";
const KEY_BUFFER: usize = 32;

pub(crate) struct PushRepository {
    pub(crate) repo: Repository,
    pub(crate) owner_email: Option<String>,
    pub(crate) organization: Option<String>,
}

impl Serialize for PushRepository {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.repo.serialize(Form {
            inner: s,
            push: self,
        })
    }
}

#[derive(Serialize)]
struct PushOwner<'a> {
    name: &'a str,
    email: Option<&'a str>,
    #[serde(flatten)]
    user: &'a SimpleUser,
}

#[derive(Clone, Copy)]
enum Field {
    Other,
    CreatedAt,
    PushedAt,
    Owner,
    Url,
    Absent,
}

fn field<K: ?Sized + Serialize>(key: &K) -> Field {
    let mut buffer = [0u8; KEY_BUFFER];
    let left = {
        let mut out = &mut buffer[..];
        if serde_json::to_writer(&mut out, key).is_err() {
            return Field::Other;
        }
        out.len()
    };
    match &buffer[..KEY_BUFFER - left] {
        b"\"created_at\"" => Field::CreatedAt,
        b"\"pushed_at\"" => Field::PushedAt,
        b"\"owner\"" => Field::Owner,
        b"\"url\"" => Field::Url,
        b"\"has_discussions\""
        | b"\"permissions\""
        | b"\"allow_rebase_merge\""
        | b"\"allow_squash_merge\""
        | b"\"allow_auto_merge\""
        | b"\"delete_branch_on_merge\""
        | b"\"allow_merge_commit\""
        | b"\"allow_forking\"" => Field::Absent,
        _ => Field::Other,
    }
}

struct Entries<'a, M> {
    inner: M,
    push: &'a PushRepository,
    field: Field,
}

impl<M: SerializeMap> SerializeMap for Entries<'_, M> {
    type Ok = M::Ok;
    type Error = M::Error;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), M::Error> {
        self.field = field(key);
        match self.field {
            Field::Absent => Ok(()),
            _ => self.inner.serialize_key(key),
        }
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), M::Error> {
        let r = &self.push.repo;
        match std::mem::replace(&mut self.field, Field::Other) {
            Field::CreatedAt => self.inner.serialize_value(&r.created_at.map(|t| t.0)),
            Field::PushedAt => self.inner.serialize_value(&r.pushed_at.map(|t| t.0)),
            Field::Owner => self.inner.serialize_value(&PushOwner {
                name: &r.owner.login,
                email: self.push.owner_email.as_deref(),
                user: &r.owner,
            }),
            Field::Url => self.inner.serialize_value(&r.html_url),
            Field::Absent => Ok(()),
            Field::Other => self.inner.serialize_value(value),
        }
    }

    fn end(mut self) -> Result<M::Ok, M::Error> {
        let r = &self.push.repo;
        self.inner
            .serialize_entry("stargazers", &r.stargazers_count)?;
        self.inner
            .serialize_entry("master_branch", &r.default_branch)?;
        if let Some(org) = &self.push.organization {
            self.inner.serialize_entry("organization", org)?;
        }
        self.inner.end()
    }
}

struct Form<'a, S> {
    inner: S,
    push: &'a PushRepository,
}

macro_rules! refuse {
    ($($name:ident($($arg:ty),*) -> $ok:ty;)*) => {$(
        fn $name(self, $(_: $arg),*) -> Result<$ok, S::Error> {
            Err(S::Error::custom(NOT_A_MAP))
        }
    )*};
}

impl<'a, S: Serializer> Serializer for Form<'a, S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Impossible<S::Ok, S::Error>;
    type SerializeTuple = Impossible<S::Ok, S::Error>;
    type SerializeTupleStruct = Impossible<S::Ok, S::Error>;
    type SerializeTupleVariant = Impossible<S::Ok, S::Error>;
    type SerializeMap = Entries<'a, S::SerializeMap>;
    type SerializeStruct = Impossible<S::Ok, S::Error>;
    type SerializeStructVariant = Impossible<S::Ok, S::Error>;

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
        Ok(Entries {
            inner: self.inner.serialize_map(None)?,
            push: self.push,
            field: Field::Other,
        })
    }

    refuse! {
        serialize_bool(bool) -> S::Ok;
        serialize_i8(i8) -> S::Ok;
        serialize_i16(i16) -> S::Ok;
        serialize_i32(i32) -> S::Ok;
        serialize_i64(i64) -> S::Ok;
        serialize_u8(u8) -> S::Ok;
        serialize_u16(u16) -> S::Ok;
        serialize_u32(u32) -> S::Ok;
        serialize_u64(u64) -> S::Ok;
        serialize_f32(f32) -> S::Ok;
        serialize_f64(f64) -> S::Ok;
        serialize_char(char) -> S::Ok;
        serialize_str(&str) -> S::Ok;
        serialize_bytes(&[u8]) -> S::Ok;
        serialize_none() -> S::Ok;
        serialize_unit() -> S::Ok;
        serialize_unit_struct(&'static str) -> S::Ok;
        serialize_unit_variant(&'static str, u32, &'static str) -> S::Ok;
        serialize_seq(Option<usize>) -> Self::SerializeSeq;
        serialize_tuple(usize) -> Self::SerializeTuple;
        serialize_tuple_struct(&'static str, usize) -> Self::SerializeTupleStruct;
        serialize_tuple_variant(&'static str, u32, &'static str, usize) -> Self::SerializeTupleVariant;
        serialize_struct(&'static str, usize) -> Self::SerializeStruct;
        serialize_struct_variant(&'static str, u32, &'static str, usize) -> Self::SerializeStructVariant;
    }

    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<S::Ok, S::Error> {
        Err(S::Error::custom(NOT_A_MAP))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<S::Ok, S::Error> {
        Err(S::Error::custom(NOT_A_MAP))
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<S::Ok, S::Error> {
        Err(S::Error::custom(NOT_A_MAP))
    }
}
