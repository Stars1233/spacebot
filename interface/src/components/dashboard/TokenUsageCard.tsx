import {useState} from "react";
import {Card, CardHeader, CardContent, Button} from "@spacedrive/primitives";

interface ModelUsage {
	model: string;
	tokens: number;
	color: string;
}

const DATA_BY_PERIOD: Record<string, {total: number; models: ModelUsage[]}> = {
	today: {
		total: 142_840,
		models: [
			{model: "claude-sonnet-4-5", tokens: 98_400, color: "#f59e0b"},
			{model: "gpt-4o", tokens: 28_340, color: "#3b82f6"},
			{model: "claude-haiku-4-5", tokens: 16_100, color: "#8b5cf6"},
		],
	},
	"7d": {
		total: 847_320,
		models: [
			{model: "claude-sonnet-4-5", tokens: 580_000, color: "#f59e0b"},
			{model: "gpt-4o", tokens: 180_000, color: "#3b82f6"},
			{model: "claude-haiku-4-5", tokens: 87_320, color: "#8b5cf6"},
		],
	},
	"30d": {
		total: 3_247_900,
		models: [
			{model: "claude-sonnet-4-5", tokens: 2_140_000, color: "#f59e0b"},
			{model: "gpt-4o", tokens: 720_000, color: "#3b82f6"},
			{model: "claude-haiku-4-5", tokens: 387_900, color: "#8b5cf6"},
		],
	},
};

function formatTokens(n: number): string {
	if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
	if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`;
	return String(n);
}

type Period = "today" | "7d" | "30d";

export function TokenUsageCard() {
	const [period, setPeriod] = useState<Period>("7d");
	const data = DATA_BY_PERIOD[period];

	return (
		<Card variant="dark">
			<CardHeader className="flex-row items-center justify-between p-4 pb-3">
				<h2 className="font-plex text-sm font-medium text-ink-dull">
					Token Usage
				</h2>
				<div className="flex items-center gap-0.5">
					{(["today", "7d", "30d"] as Period[]).map((p) => (
						<Button
							key={p}
							size="xs"
							variant={period === p ? "gray" : "subtle"}
							onClick={() => setPeriod(p)}
						>
							{p}
						</Button>
					))}
				</div>
			</CardHeader>

			<CardContent className="px-4 pb-4 pt-0">
				<div className="mb-4">
					<span className="font-plex text-2xl font-semibold tabular-nums text-ink">
						{formatTokens(data.total)}
					</span>
					<span className="ml-1.5 text-sm text-ink-faint">tokens</span>
				</div>

				<div className="flex flex-col gap-3">
					{data.models.map((m) => {
						const pct = (m.tokens / data.total) * 100;
						return (
							<div key={m.model}>
								<div className="mb-1 flex items-center justify-between">
									<span className="text-tiny text-ink-dull">{m.model}</span>
									<span className="tabular-nums text-tiny text-ink-faint">
										{formatTokens(m.tokens)}
									</span>
								</div>
								<div className="h-1.5 w-full overflow-hidden rounded-full bg-app-selected/40">
									<div
										className="h-full rounded-full transition-all duration-500"
										style={{width: `${pct}%`, backgroundColor: m.color}}
									/>
								</div>
							</div>
						);
					})}
				</div>
			</CardContent>
		</Card>
	);
}
