interface User {
  id: number;
  name: string;
}

type ID<T> = T;

function identity<T>(value: T): ID<T> {
  return value as ID<T>;
}

const user: User = {
  id: 42,
  name: "alice",
};

const result = identity<string>(user.name);
console.log(result);
