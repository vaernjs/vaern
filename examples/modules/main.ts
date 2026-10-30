// Entry point executing relative ES module import and calculation.
import { add, formatResult } from "./math.ts";

const sum = add(40, 2);
console.log(formatResult("sum", sum));

