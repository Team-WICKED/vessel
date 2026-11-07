import { loadEnv, stopFleet } from "./fleet";

export default function globalTeardown(): void {
  stopFleet(loadEnv());
}
