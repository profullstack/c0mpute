import { redirect } from "next/navigation";

// "my dashboard": storage (sites + disk) first; compute is one tab over.
export default function DashboardIndex() {
  redirect("/dashboard/disk");
}
