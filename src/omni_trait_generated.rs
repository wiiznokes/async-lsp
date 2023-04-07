// This file is automatically @generated by update_omni_trait.sh
// It is not intended for manual editing.
#[rustfmt::ignore]
define! {
    // Client -> Server requests.
    {
        "textDocument/implementation", implementation;
        "textDocument/typeDefinition", type_definition;
        "textDocument/documentColor", document_color;
        "textDocument/colorPresentation", color_presentation;
        "textDocument/foldingRange", folding_range;
        "textDocument/declaration", declaration;
        "textDocument/selectionRange", selection_range;
        "textDocument/prepareCallHierarchy", prepare_call_hierarchy;
        "callHierarchy/incomingCalls", incoming_calls;
        "callHierarchy/outgoingCalls", outgoing_calls;
        "textDocument/semanticTokens/full", semantic_tokens_full;
        "textDocument/semanticTokens/full/delta", semantic_tokens_full_delta;
        "textDocument/semanticTokens/range", semantic_tokens_range;
        "textDocument/linkedEditingRange", linked_editing_range;
        "workspace/willCreateFiles", will_create_files;
        "workspace/willRenameFiles", will_rename_files;
        "workspace/willDeleteFiles", will_delete_files;
        "textDocument/moniker", moniker;
        "textDocument/prepareTypeHierarchy", prepare_type_hierarchy;
        "typeHierarchy/supertypes", supertypes;
        "typeHierarchy/subtypes", subtypes;
        "textDocument/inlineValue", inline_value;
        "textDocument/inlayHint", inlay_hint;
        "inlayHint/resolve", inlay_hint_resolve;
        "textDocument/willSaveWaitUntil", will_save_wait_until;
        "textDocument/completion", completion;
        "completionItem/resolve", completion_item_resolve;
        "textDocument/hover", hover;
        "textDocument/signatureHelp", signature_help;
        "textDocument/definition", definition;
        "textDocument/references", references;
        "textDocument/documentHighlight", document_highlight;
        "textDocument/documentSymbol", document_symbol;
        "textDocument/codeAction", code_action;
        "codeAction/resolve", code_action_resolve;
        "workspace/symbol", symbol;
        "textDocument/codeLens", code_lens;
        "codeLens/resolve", code_lens_resolve;
        "textDocument/documentLink", document_link;
        "documentLink/resolve", document_link_resolve;
        "textDocument/formatting", formatting;
        "textDocument/rangeFormatting", range_formatting;
        "textDocument/onTypeFormatting", on_type_formatting;
        "textDocument/rename", rename;
        "textDocument/prepareRename", prepare_rename;
        "workspace/executeCommand", execute_command;
    }
    // Client -> Server notifications.
    {
        "workspace/didChangeWorkspaceFolders", did_change_workspace_folders;
        "window/workDoneProgress/cancel", work_done_progress_cancel;
        "workspace/didCreateFiles", did_create_files;
        "workspace/didRenameFiles", did_rename_files;
        "workspace/didDeleteFiles", did_delete_files;
        "workspace/didChangeConfiguration", did_change_configuration;
        "textDocument/didOpen", did_open;
        "textDocument/didChange", did_change;
        "textDocument/didClose", did_close;
        "textDocument/didSave", did_save;
        "textDocument/willSave", will_save;
        "workspace/didChangeWatchedFiles", did_change_watched_files;
        "$/setTrace", set_trace;
        "$/cancelRequest", cancel_request;
        "$/progress", progress;
    }
    // Server -> Client requests.
    {
        "workspace/workspaceFolders", workspace_folders;
        "workspace/configuration", configuration;
        "window/workDoneProgress/create", work_done_progress_create;
        "workspace/semanticTokens/refresh", semantic_tokens_refresh;
        "window/showDocument", show_document;
        "workspace/inlineValue/refresh", inline_value_refresh;
        "workspace/inlayHint/refresh", inlay_hint_refresh;
        "client/registerCapability", register_capability;
        "client/unregisterCapability", unregister_capability;
        "window/showMessageRequest", show_message_request;
        "workspace/codeLens/refresh", code_lens_refresh;
        "workspace/applyEdit", apply_edit;
    }
    // Server -> Client notifications.
    {
        "window/showMessage", show_message;
        "window/logMessage", log_message;
        "telemetry/event", telemetry_event;
        "textDocument/publishDiagnostics", publish_diagnostics;
        "$/logTrace", log_trace;
        "$/cancelRequest", cancel_request;
        "$/progress", progress;
    }
}