// Starts the fleet once for every test (see fleet.ts).
import { saveEnv, startFleet } from "./fleet";

export default async function globalSetup(): Promise<void> {
  saveEnv(await startFleet());
}
