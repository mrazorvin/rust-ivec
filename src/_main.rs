#![feature(trait_alias)]

use std::{array, mem};

use jzon::{stringify_pretty, JsonValue};
use mlua::{
    chunk, Function, Lua, MetaMethod, UserDataMethods,
    Value::{self},
};

struct JSONPairs {
    json: &'static JsonValue,
    i: usize,
}

trait Bar<A> = Iterator<Item = IntoIterator<Item = A, IntoIter = Iterator<A>>>

fn help<T: IntoIterator, O: Bar<T>>(mut xxx: O) -> T::Item {
    xxx.next().unwrap().into_iter().next().unwrap()
}

fn main() {
    print!("{}", help(vec![vec![1]].into_iter()));

    let mut data = jzon::object! {
        foo: true,
        bar: null,
        answer: 42,
        list: ["world", true],
        my_value: "My Value"
    };

    let lua = Lua::new();

    let _ = lua.register_userdata_type::<JSONPairs>(|reg| {
        reg.add_meta_method_mut(MetaMethod::Call, |lua, this, ()| match this.json {
            JsonValue::Object(object) => {
                let pairs = this
                    .json
                    .as_object()
                    .unwrap_or_else(|| panic!("must be object"))
                    .skip(this.i)
                    .take(1)
                    .map(|(k, _)| Value::String(lua.create_string(k).unwrap()))
                    .next();

                match pairs {
                    Some(v) => {
                        this.i += 1;
                        Ok(v)
                    }
                    None => Ok(Value::Nil),
                }
            }
            JsonValue::Array(array) => {
                if this.i < array.len() {
                    let val = Ok(Value::Number(this.i as f64));
                    this.i += 1;
                    return val;
                }

                Ok(Value::Nil)
            }
            _ => Ok(Value::Nil),
        });
    });

    let _ = lua.register_userdata_type::<&jzon::JsonValue>(|reg| {
        reg.add_meta_method(MetaMethod::Index, |lua, this, value: Value| match value {
            Value::String(propery) => {
                let json = &this[propery.to_str()?];

                return match json {
                    jzon::JsonValue::Boolean(boolean) => Result::Ok(Value::Boolean(*boolean)),
                    jzon::JsonValue::Number(num) => Result::Ok(Value::Number(f64::from(*num))),
                    jzon::JsonValue::String(string) => {
                        Result::Ok(Value::String(lua.create_string(&string).unwrap()))
                    }
                    jzon::JsonValue::Short(string) => {
                        Result::Ok(Value::String(lua.create_string(string.as_str()).unwrap()))
                    }
                    _ => Result::Ok(Value::UserData(lua.create_any_userdata(json).unwrap())),
                };
            }
            Value::Integer(int) => match this {
                JsonValue::Array(array) => match array.get(int as usize) {
                    Some(value) => Ok(Value::UserData(lua.create_any_userdata(value).unwrap())),
                    _ => Ok(Value::Nil),
                },
                _ => unimplemented!(),
            },
            _ => unimplemented!(),
        });

        reg.add_meta_method(MetaMethod::ToString, |lua, this, ()| {
            Result::Ok(Value::String(
                lua.create_string(stringify_pretty((*this).clone(), 2))
                    .unwrap(),
            ))
        });

        reg.add_meta_method(MetaMethod::Iter, |lua, this, ()| {
            Ok(lua.create_any_userdata(JSONPairs { i: 0, json: *this }))
        });
    });

    let arg = lua.create_any_userdata(unsafe { mem::transmute::<_, &'static JsonValue>(&data) });

    let main: Function = lua
        .load(chunk! {
            return function(arg)
                for key in arg.list do
                    print(arg.list[key])
                end
            end
        })
        .eval()
        .unwrap();

    let _result: Value = main.call(arg).unwrap();
}
