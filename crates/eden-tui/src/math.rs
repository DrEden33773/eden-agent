//! Unicode math notation without executing TeX commands or discarding unknown syntax.
pub(super) fn render(source: &str) -> Option<String> {
    if source.len() > 32_000 {
        return None;
    }
    let mut parser = Math {
        input: source,
        depth: 0,
    };
    let result = parser.expression(false)?;
    (!result.trim().is_empty()).then_some(result)
}
struct Math<'a> {
    input: &'a str,
    depth: usize,
}
impl Math<'_> {
    fn next(&mut self) -> Option<char> {
        let ch = self.input.chars().next()?;
        self.input = &self.input[ch.len_utf8()..];
        Some(ch)
    }
    fn group(&mut self) -> Option<String> {
        self.input = self.input.trim_start();
        if self.next()? != '{' {
            return None;
        }
        self.expression(true)
    }
    fn expression(&mut self, grouped: bool) -> Option<String> {
        self.depth += 1;
        if self.depth > 32 {
            return None;
        }
        let mut output = String::new();
        while let Some(ch) = self.next() {
            match ch {
                '}' => {
                    if !grouped {
                        return None;
                    }
                    self.depth -= 1;
                    return Some(output);
                }
                '{' => output.push_str(&self.expression(true)?),
                '^' | '_' => {
                    let value = if self.input.starts_with('{') {
                        self.group()?
                    } else {
                        {
                            let atom = self.next()?;
                            if !atom.is_alphanumeric() {
                                return None;
                            }
                            atom.to_string()
                        }
                    };
                    let alphabet = if ch == '^' {
                        "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿⁱ"
                    } else {
                        "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₙᵢ"
                    };
                    let mapped: Option<String> = value
                        .chars()
                        .map(|c| {
                            "0123456789+-=()ni"
                                .chars()
                                .position(|base| base == c)
                                .and_then(|index| alphabet.chars().nth(index))
                        })
                        .collect();
                    if let Some(mapped) = mapped {
                        output.push_str(&mapped);
                    } else {
                        output.push(ch);
                        output.push('(');
                        output.push_str(&value);
                        output.push(')');
                    }
                }
                '\\' => {
                    let length = self
                        .input
                        .bytes()
                        .take_while(u8::is_ascii_alphabetic)
                        .count();
                    let command = &self.input[..length];
                    self.input = &self.input[length..];
                    match command {
                        "frac" | "dfrac" | "tfrac" => {
                            let numerator = self.group()?;
                            let denominator = self.group()?;
                            output.push_str(&format!("({numerator})/({denominator})"));
                        }
                        "sqrt" => output.push_str(&format!("√({})", self.group()?)),
                        "text" | "mathrm" | "mathbf" => output.push_str(&self.group()?),
                        "left" | "right" => {}
                        "" => match self.next()? {
                            ' ' | ',' | ';' | '!' => output.push(' '),
                            c @ ('{' | '}' | '%' | '_' | '$') => output.push(c),
                            _ => return None,
                        },
                        _ => output.push_str(match command {
                            "alpha" => "α",
                            "beta" => "β",
                            "gamma" => "γ",
                            "delta" => "δ",
                            "epsilon" => "ε",
                            "theta" => "θ",
                            "lambda" => "λ",
                            "mu" => "μ",
                            "pi" => "π",
                            "sigma" => "σ",
                            "phi" => "φ",
                            "omega" => "ω",
                            "Delta" => "Δ",
                            "Sigma" => "Σ",
                            "Omega" => "Ω",
                            "sum" => "∑",
                            "prod" => "∏",
                            "int" => "∫",
                            "infty" => "∞",
                            "partial" => "∂",
                            "nabla" => "∇",
                            "times" => "×",
                            "cdot" => "·",
                            "pm" => "±",
                            "le" | "leq" => "≤",
                            "ge" | "geq" => "≥",
                            "ne" | "neq" => "≠",
                            "approx" => "≈",
                            "to" | "rightarrow" => "→",
                            "in" => "∈",
                            "sin" => "sin",
                            "cos" => "cos",
                            "tan" => "tan",
                            "log" => "log",
                            "ln" => "ln",
                            "lim" => "lim",
                            _ => return None,
                        }),
                    }
                }
                '$' | '&' | '#' => return None,
                ch => output.push(ch),
            }
        }
        self.depth -= 1;
        (!grouped).then_some(output)
    }
}
