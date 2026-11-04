// console.log of an Error prints its name, message, frames and cause.
const err = new Error("boom", { cause: new TypeError("inner") });
console.log(err);
