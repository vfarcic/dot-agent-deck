import { writeFileSync } from "node:fs";

// No model turn: this slash command exercises another extension's real UI.
export default function (pi: any) {
  pi.registerCommand("question-select", {
    description: "Open the deterministic question test dialog",
    handler: async (_args: string, ctx: any) => {
      const result = await ctx.ui.select("question_pi_186c5a9f colour", ["Red", "Green", "Blue"]);
      writeFileSync(process.env.QUESTION_RESULT_FILE!, JSON.stringify(result));
      ctx.ui.notify(`Selected ${result}`, "info");
    },
  });
}
