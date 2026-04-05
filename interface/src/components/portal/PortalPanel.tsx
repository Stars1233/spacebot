import { useEffect, useRef, useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { usePortal, getPortalSessionId } from "@/hooks/usePortal";
import { useLiveContext } from "@/hooks/useLiveContext";
import { api, type ConversationDefaultsResponse, type ConversationSettings } from "@/api/client";
import { PortalHeader } from "./PortalHeader";
import { PortalTimeline } from "./PortalTimeline";
import { PortalComposer } from "./PortalComposer";
import { PortalEmpty } from "./PortalEmpty";

interface PortalPanelProps {
	agentId: string;
}

export function PortalPanel({ agentId }: PortalPanelProps) {
	const queryClient = useQueryClient();
	const [activeConversationId, setActiveConversationId] = useState<string>(
		getPortalSessionId(agentId),
	);
	const { isSending, error, sendMessage } = usePortal(agentId, activeConversationId);
	const { liveStates } = useLiveContext();
	const [input, setInput] = useState("");
	const [showSettings, setShowSettings] = useState(false);
	const [showHistory, setShowHistory] = useState(false);
	const [settings, setSettings] = useState<ConversationSettings>({});
	const [pendingFiles, setPendingFiles] = useState<File[]>([]);
	// Track uploaded attachment IDs keyed by file name+size for deduplication.
	const uploadedIds = useRef<Map<string, string>>(new Map());

	// Fetch conversations list
	const { data: conversationsData } = useQuery({
		queryKey: ["portal-conversations", agentId],
		queryFn: async () => {
			const response = await api.listPortalConversations(agentId);
			if (!response.ok) throw new Error(`HTTP ${response.status}`);
			return response.json();
		},
	});

	const conversations = conversationsData?.conversations ?? [];

	// Auto-select the newest conversation on first load
	useEffect(() => {
		if (conversations.length === 0) return;
		const isPlaceholder = activeConversationId === getPortalSessionId(agentId);
		if (!isPlaceholder) return;
		const newest = conversations[0];
		if (newest) setActiveConversationId(newest.id);
	}, [conversationsData, agentId]);

	// Reset settings when switching conversations, hydrating from cached data
	useEffect(() => {
		const activeConv = conversations.find(
			(c: { id: string; settings?: ConversationSettings }) => c.id === activeConversationId,
		);
		setSettings(activeConv?.settings ?? {});
		setShowSettings(false);
	}, [activeConversationId, agentId, conversationsData]);

	const {
		data: defaults,
		isLoading: defaultsLoading,
		error: defaultsError,
	} = useQuery<ConversationDefaultsResponse>({
		queryKey: ["conversation-defaults", agentId],
		queryFn: () => api.getConversationDefaults(agentId),
	});

	const { data: projectsData } = useQuery({
		queryKey: ["projects"],
		queryFn: () => api.listProjects("active"),
		staleTime: 30_000,
	});
	const projects = projectsData?.projects ?? [];
	const projectOptions = projects.map((p) => p.name);
	const [selectedProject, setSelectedProject] = useState<string>("");
	useEffect(() => {
		if (!selectedProject && projectOptions.length > 0) {
			setSelectedProject(projectOptions[0]);
		}
	}, [projectOptions, selectedProject]);

	const agentsQuery = useQuery({
		queryKey: ["agents"],
		queryFn: () => api.agents(),
		staleTime: 10_000,
	});
	const agentDisplayName =
		agentsQuery.data?.agents.find((a) => a.id === agentId)?.display_name ?? agentId;

	const liveState = liveStates[activeConversationId];
	const timeline = liveState?.timeline ?? [];
	const isTyping = liveState?.isTyping ?? false;
	const activeWorkers = Object.values(liveState?.workers ?? {});

	const createConversationMutation = useMutation({
		mutationFn: async () => {
			const response = await api.createPortalConversation(agentId);
			if (!response.ok) throw new Error(`HTTP ${response.status}`);
			return response.json();
		},
		onSuccess: (data) => {
			setActiveConversationId(data.conversation.id);
			queryClient.invalidateQueries({ queryKey: ["portal-conversations", agentId] });
		},
	});

	const deleteConversationMutation = useMutation({
		mutationFn: async (id: string) => {
			const response = await api.deletePortalConversation(agentId, id);
			if (!response.ok) throw new Error(`HTTP ${response.status}`);
			return response.json();
		},
		onSuccess: (_, deletedId) => {
			if (activeConversationId === deletedId) {
				setActiveConversationId(getPortalSessionId(agentId));
			}
			queryClient.invalidateQueries({ queryKey: ["portal-conversations", agentId] });
		},
	});

	const archiveConversationMutation = useMutation({
		mutationFn: async ({ id, archived }: { id: string; archived: boolean }) => {
			const response = await api.updatePortalConversation(agentId, id, undefined, archived);
			if (!response.ok) throw new Error(`HTTP ${response.status}`);
			return response.json();
		},
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["portal-conversations", agentId] });
		},
	});

	const saveSettingsMutation = useMutation({
		mutationFn: async () => {
			if (!activeConversationId) return;
			const response = await api.updatePortalConversation(
				agentId,
				activeConversationId,
				undefined,
				undefined,
				settings,
			);
			if (!response.ok) throw new Error(`HTTP ${response.status}`);
			return response.json();
		},
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["portal-conversations", agentId] });
			setShowSettings(false);
		},
	});

	const handleAddFiles = (files: File[]) => {
		setPendingFiles((prev) => [...prev, ...files]);
	};

	const handleRemoveFile = (index: number) => {
		setPendingFiles((prev) => prev.filter((_, i) => i !== index));
	};

	const handleSubmit = async () => {
		const trimmed = input.trim();
		if (!trimmed || isSending) return;
		setInput("");

		// Upload any pending files and collect their IDs.
		let attachmentIds: string[] = [];
		if (pendingFiles.length > 0) {
			const filesToUpload = pendingFiles;
			setPendingFiles([]);

			const ids = await Promise.all(
				filesToUpload.map(async (file) => {
					const key = `${file.name}:${file.size}`;
					const cached = uploadedIds.current.get(key);
					if (cached) return cached;

					try {
						const response = await api.uploadAttachment(agentId, activeConversationId, file);
						if (!response.ok) return null;
						const data: { id: string } = await response.json();
						uploadedIds.current.set(key, data.id);
						return data.id;
					} catch {
						return null;
					}
				}),
			);
			attachmentIds = ids.filter((id): id is string => id !== null);
		}

		sendMessage(trimmed, attachmentIds.length > 0 ? attachmentIds : undefined);
	};

	const modelLabel = defaults
		? (defaults.available_models.find(
				(m) => m.id === (settings.model || defaults.model),
			)?.name ?? settings.model ?? defaults.model)
		: undefined;

	const isEmpty = timeline.length === 0 && !isTyping;

	return (
		<div className="relative flex h-full w-full min-w-0 flex-col">
				<PortalHeader
					title={agentDisplayName}
					modelLabel={modelLabel}
					responseMode={settings.response_mode}
					activeWorkers={activeWorkers}
					showSettings={showSettings}
					onToggleSettings={setShowSettings}
					defaults={defaults}
					defaultsLoading={defaultsLoading}
					defaultsError={defaultsError as Error | null}
					settings={settings}
					onSettingsChange={setSettings}
					onSaveSettings={() => saveSettingsMutation.mutate()}
					saving={saveSettingsMutation.isPending}
					conversations={conversations}
					activeConversationId={activeConversationId}
					onNewConversation={() => createConversationMutation.mutate()}
					onSelectConversation={setActiveConversationId}
					onDeleteConversation={(id) => deleteConversationMutation.mutate(id)}
					onArchiveConversation={(id, archived) =>
						archiveConversationMutation.mutate({ id, archived })
					}
					showHistory={showHistory}
					onToggleHistory={setShowHistory}
				/>

				{isEmpty ? (
					<div className="flex flex-1 items-center justify-center py-10">
						<div className="w-full max-w-2xl px-6">
							<PortalEmpty agentName={agentDisplayName} />
							<PortalComposer
								agentName={agentDisplayName}
								draft={input}
								onDraftChange={setInput}
								onSend={() => void handleSubmit()}
								disabled={isSending || isTyping}
								modelOptions={defaults?.available_models ?? []}
								selectedModel={settings.model || defaults?.model || ""}
								onSelectModel={(model) => setSettings((s) => ({ ...s, model }))}
								projectOptions={projectOptions}
								selectedProject={selectedProject}
								onSelectProject={setSelectedProject}
								pendingFiles={pendingFiles}
								onAddFiles={handleAddFiles}
								onRemoveFile={handleRemoveFile}
							/>
						</div>
					</div>
				) : (
					<>
						<PortalTimeline
							agentId={agentId}
							conversationId={activeConversationId}
							timeline={timeline}
							isTyping={isTyping}
						/>

						{error && (
							<div className="mx-4 mb-2 rounded-lg border border-red-500/20 bg-red-500/5 px-4 py-3 text-sm text-red-400">
								{error}
							</div>
						)}

						<div className="absolute inset-x-0 bottom-0 z-10 p-4 bg-gradient-to-t from-app via-app/80 to-transparent pt-8 pointer-events-none">
							<div className="mx-auto w-full max-w-3xl pointer-events-auto">
								<PortalComposer
									agentName={agentDisplayName}
									draft={input}
									onDraftChange={setInput}
									onSend={() => void handleSubmit()}
									disabled={isSending || isTyping}
									modelOptions={defaults?.available_models ?? []}
									selectedModel={settings.model || defaults?.model || ""}
									onSelectModel={(model) => setSettings((s) => ({ ...s, model }))}
									projectOptions={projectOptions}
									selectedProject={selectedProject}
									onSelectProject={setSelectedProject}
									pendingFiles={pendingFiles}
									onAddFiles={handleAddFiles}
									onRemoveFile={handleRemoveFile}
								/>
							</div>
						</div>
					</>
				)}
		</div>
	);
}
