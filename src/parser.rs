use lexer::tokens::{Token, FlagType};
use lexer::{Lexer, TokenIterator};
use std::iter;

pub enum Expr {
    Num(f64),
    Var(Var),
    BinOp(FlagType, Box<Expr>, Box<Expr>),
    UnaryOp(FlagType, Box<Expr>),
}

pub enum Var {
    Name(String),
}

pub enum Stat {
    Empty,
    Assign(Vec<Var>, Vec<Box<Expr>>),
}

pub enum Node {
    Expr(Expr),
    Block(Vec<Box<Stat>>),
}

#[derive(Debug)]
pub enum ParserError {
    SyntaxError,
}

pub struct Parser<'a, Tit>
    where Tit: iter::Iterator<Item = char> + Clone
{
    /// peekable tokenizer
    token_iter: iter::Peekable<TokenIterator<'a, Tit>>,
}

/// Common method
impl<'a, Tit> Parser<'a, Tit>
    where Tit: iter::Iterator<Item = char> + Clone
{
    pub fn parse(text: TokenIterator<'a, Tit>) -> Result<Box<Node>, ParserError> {
        let mut obj = Parser { token_iter: text.peekable() };
        obj.program()
    }

    pub fn peek_clone(&mut self) -> Option<Token> {
        self.token_iter.peek().map(|t| {
            println!("{:?}", t);
            t.clone()
        })
    }

    /// compare the current token with the passed token
    /// if they match, advance tokenizer
    /// otherwise raise an error
    fn eat(&mut self, ft: FlagType) -> Result<(), ParserError> {
        if let Some(true) = self.token_iter.peek().map(|token| {
            // map: end reference scope
            match *token {
                Token::Num(_) => ft == FlagType::Integer,
                Token::Name(_) => ft == FlagType::Name,
                Token::Str(_) => ft == FlagType::Str,
                Token::Flag(t) => ft == t,
            }
        }) {
            self.token_iter.next();
            Ok(())
        } else {
            Err(ParserError::SyntaxError)
        }
    }
}

/// Expression parser
impl<'a, Tit> Parser<'a, Tit>
    where Tit: iter::Iterator<Item = char> + Clone
{
    /// rule: factor: (Plus | Minus) factor | Integer | LParen expr RParen | Var
    fn factor(&mut self) -> Result<Box<Expr>, ParserError> {
        if let Some(token) = self.peek_clone() {
            match token {
                Token::Num(n) => {
                    self.eat(FlagType::Integer);
                    Ok(Box::new(Expr::Num(n)))
                }
                Token::Flag(t) if t == FlagType::Plus || t == FlagType::Minus => {
                    self.eat(t);
                    let node = try!(self.factor());
                    Ok(Box::new(Expr::UnaryOp(t, node)))
                }
                Token::Flag(FlagType::LParen) => {
                    self.eat(FlagType::LParen);
                    let node = try!(self.expr());
                    try!(self.eat(FlagType::RParen));
                    Ok(node)
                }
                Token::Name(id) => {
                    self.eat(FlagType::Name);
                    Ok(Box::new(Expr::Var(Var::Name(id))))
                }
                _ => Err(ParserError::SyntaxError),
            }
        } else {
            Err(ParserError::SyntaxError) // e.g. ended early
        }

    }

    /// rule: term : factor((Mul | Div) factor)*
    fn term(&mut self) -> Result<Box<Expr>, ParserError> {
        let mut node = try!(self.factor());
        while let Some(token) = self.token_iter.peek().map(|t| t.clone()) {
            match token {
                Token::Flag(FlagType::Mul) => {
                    self.eat(FlagType::Mul); // must succeed
                    node = Box::new(Expr::BinOp(FlagType::Mul, node, try!(self.factor())));
                }
                Token::Flag(FlagType::Div) => {
                    self.eat(FlagType::Div); // must secceed
                    node = Box::new(Expr::BinOp(FlagType::Div, node, try!(self.factor())));
                }
                _ => break, 
            }
        }
        Ok(node)
    }

    /// rule: expr : term((Plus | Minus) term)*
    fn expr(&mut self) -> Result<Box<Expr>, ParserError> {
        let mut node = try!(self.term());
        while let Some(token) = self.token_iter.peek().map(|t| t.clone()) {
            match token {
                Token::Flag(FlagType::Plus) => {
                    self.eat(FlagType::Plus);
                    node = Box::new(Expr::BinOp(FlagType::Plus, node, try!(self.term())));
                }
                Token::Flag(FlagType::Minus) => {
                    self.eat(FlagType::Minus);
                    node = Box::new(Expr::BinOp(FlagType::Minus, node, try!(self.term())));
                }
                _ => break,
            };
        }
        Ok(node)
    }

    fn error() {
        panic!("Unexpected token");
    }
}

/// Statement parser
impl<'a, Tit> Parser<'a, Tit>
    where Tit: iter::Iterator<Item = char> + Clone
{
    /// rule: program: Block [newline]
    fn program(&mut self) -> Result<Box<Node>, ParserError> {
        self.block()
        // TODO: handle newline
    }

    /// rule: Block: {Stat} [Retstat]
    fn block(&mut self) -> Result<Box<Node>, ParserError> {
        let mut stats: Vec<Box<Stat>> = vec![];
        while let Ok(stat) = self.stat() {
            stats.push(stat);
        }
        Ok(Box::new(Node::Block(stats)))
        // TODO: Retstat
    }

    /// rule: Stat: Colons | (Varlist Assign ExprList)
    fn stat(&mut self) -> Result<Box<Stat>, ParserError> {
        if let Some(Token::Flag(FlagType::Colons)) = self.peek_clone() {
            Ok(Box::new(Stat::Empty))
        } else {
            let mut varlist = try!(self.varlist());
            self.eat(FlagType::Assign);
            let mut exprlist = try!(self.exprlist());
            if varlist.len() == exprlist.len() {
                Ok(Box::new(Stat::Assign(varlist, exprlist)))
            } else {
                Err(ParserError::SyntaxError)
            }
        }
    }

    /// rule: var: Name | PrefixExpr |
    fn var(&mut self) -> Result<Var, ParserError> {
        if let Some(Token::Name(id)) = self.peek_clone() {
            self.eat(FlagType::Name);
            Ok(Var::Name(id))
        } else {
            Err(ParserError::SyntaxError)
        }
    }
    /// rule: varlist Name { Comma Name}
    fn varlist(&mut self) -> Result<Vec<Var>, ParserError> {
        let mut var = try!(self.var());
        let mut list = vec![var];
        while let Some(token) = self.peek_clone() {
            match token {
                Token::Flag(FlagType::Comma) => {
                    self.eat(FlagType::Comma);
                    var = try!(self.var());
                    list.push(var);
                }
                _ => break,
            }
        }
        Ok(list)
    }

    /// rule: exprlist: Expr { Comma Expr}
    fn exprlist(&mut self) -> Result<Vec<Box<Expr>>, ParserError> {
        let mut expr = try!(self.expr());
        let mut list = vec![expr];
        while let Some(token) = self.peek_clone() {
            match token {
                Token::Flag(FlagType::Comma) => {
                    self.eat(FlagType::Comma);
                    expr = try!(self.expr());
                    list.push(expr);
                }
                _ => break,
            }
        }
        Ok(list)
    }
}
