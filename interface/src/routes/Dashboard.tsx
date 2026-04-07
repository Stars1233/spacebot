import { ActionItemsCard } from "@/components/dashboard/ActionItemsCard";
import { TokenUsageCard } from "@/components/dashboard/TokenUsageCard";
import { RecentActivityCard } from "@/components/dashboard/RecentActivityCard";


export function Dashboard() {
	return (
		<div className="flex h-full flex-col">
			<div className="flex-1 overflow-y-auto">
				<div className="mx-auto max-w-6xl py-3 pr-3 pb-12">
					<div className="grid grid-cols-2 gap-5">
						<div className="row-span-2">
							<ActionItemsCard />
						</div>
						<TokenUsageCard />
					</div>

					{/* Recent Activity: full width */}
					<div className="mt-5">
						<RecentActivityCard />
					</div>
				</div>
			</div>
		</div>
	);
}
