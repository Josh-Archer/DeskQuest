package com.linuxquest.client;

import android.app.Activity;
import android.content.Context;
import android.graphics.Color;
import android.text.Editable;
import android.text.InputType;
import android.view.View;
import android.view.ViewGroup;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;

/** Hidden text editor used to give Horizon OS a real IME input connection. */
public final class ImeBridge {
    private static EditText editor;

    private ImeBridge() {}

    public static void setVisible(Activity activity, boolean visible) {
        if (editor == null) {
            editor = new EditText(activity);
            editor.setSingleLine(false);
            editor.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE);
            editor.setTextColor(Color.TRANSPARENT);
            editor.setBackgroundColor(Color.TRANSPARENT);
            editor.setAlpha(0.01f);
            editor.setImportantForAutofill(View.IMPORTANT_FOR_AUTOFILL_NO);
            editor.setImeOptions(android.view.inputmethod.EditorInfo.IME_ACTION_NONE
                    | android.view.inputmethod.EditorInfo.IME_FLAG_NO_EXTRACT_UI);
            activity.addContentView(editor, new ViewGroup.LayoutParams(1, 1));
        }

        InputMethodManager imm = (InputMethodManager) activity.getSystemService(Context.INPUT_METHOD_SERVICE);
        if (visible) {
            editor.setVisibility(View.VISIBLE);
            editor.requestFocus();
            imm.showSoftInput(editor, InputMethodManager.SHOW_IMPLICIT);
        } else {
            imm.hideSoftInputFromWindow(editor.getWindowToken(), 0);
            editor.setText("");
            editor.setVisibility(View.INVISIBLE);
            activity.getWindow().getDecorView().requestFocus();
        }
    }

    /** Returns committed text, omitting the IME's current composing region. */
    public static String committedText(Activity activity) {
        if (editor == null) return "";
        Editable value = editor.getText();
        int start = BaseInputConnection.getComposingSpanStart(value);
        int end = BaseInputConnection.getComposingSpanEnd(value);
        if (start < 0 || end < 0 || start >= end) return value.toString();
        return value.subSequence(0, start).toString() + value.subSequence(end, value.length());
    }
}
