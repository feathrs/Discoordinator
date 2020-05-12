// Parses command-line-like argument strings.
use std::collections::HashMap;
use std::str::Chars;
use logos::Logos;

#[derive(Logos)]
enum Tokens<'source> {
    #[regex("[\n\t \r]+", logos::skip)]
    #[error]
    Error,

    #[regex("\\-[a-zA-Z]+", |lex| lex.slice()[1..].chars())]
    Flags(Chars<'source>),

    #[regex("\"[^\"]*\"", |lex| {let slice = lex.slice(); &slice[1..slice.len()-1]})]
    #[regex("[^\n\t \r\"]+")]
    ArgStr(&'source str),

    #[regex("\\-\\-[a-zA-Z]+", |lex| &lex.slice()[2..])]
    LongFlag(&'source str)
}

pub struct Args<'a> {
    pub kwargs: HashMap<&'a str, &'a str>,
    pub args: Vec<&'a str>,
    pub flags: HashMap<char, u8>
}

impl<'a> Args<'a> {
    pub fn parse(s: &'a str) -> Result<Self, ()> {
        let mut lexer = Tokens::lexer(s);
        let mut flags: HashMap<char, u8> = HashMap::new();
        let mut args: Vec<&str> = Vec::new();
        let mut kwargs: HashMap<&str, &str> = HashMap::new();
        while let Some(tok) = lexer.next() {
            match tok {
                Tokens::Flags(chars) => {
                    for c in chars {
                        *flags.entry(c).or_insert(0) += 1;
                    }
                },
                Tokens::ArgStr(arg) => args.push(arg),
                Tokens::LongFlag(flag) => {
                    if let Some(Tokens::ArgStr(arg)) = lexer.next() {
                        kwargs.insert(flag, arg);
                    } else {
                        return Err(())
                    }
                },
                _ => return Err(())
            }
        }
        Ok(Args {
            kwargs, args, flags
        })
    }
}